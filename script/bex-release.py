#!/usr/bin/env python3

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib

REPOSITORY = "bex-co/bex-desktop"
VERSION = re.compile(r"v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)\Z")
ARCHITECTURES = ("aarch64", "x86_64")
SYSTEMS = ("macos", "linux", "windows")


def run(*arguments):
    return subprocess.check_output(arguments, text=True).strip()


def version_tuple(tag):
    match = VERSION.fullmatch(tag)
    if not match:
        raise ValueError("Release tag must be v<major>.<minor>.<patch> without leading zeros")
    return tuple(int(component) for component in match.groups())


def validate_tag(tag):
    version_tuple(tag)
    with Path("crates/zed/Cargo.toml").open("rb") as source:
        version = tomllib.load(source)["package"]["version"]
    if tag != f"v{version}":
        raise ValueError(f"Tag {tag} does not match crates/zed/Cargo.toml version {version}")
    commit = run("git", "rev-parse", "HEAD")
    if run("git", "rev-parse", f"refs/tags/{tag}^{{commit}}") != commit:
        raise ValueError("Checkout is not the tagged commit")
    run("git", "merge-base", "--is-ancestor", commit, "origin/main")
    return version


def release_list():
    pages = json.loads(run("gh", "api", f"repos/{REPOSITORY}/releases?per_page=100", "--paginate", "--slurp"))
    return [release for page in pages for release in page]


def validate_release(tag, releases):
    candidate = version_tuple(tag)
    for release in releases:
        if release["tag_name"] == tag and not release["draft"]:
            raise ValueError(f"Release {tag} is already published; never overwrite a release")
        if not release["draft"] and not release["prerelease"] and VERSION.fullmatch(release["tag_name"]):
            if version_tuple(release["tag_name"]) >= candidate:
                raise ValueError("Release version must be newer than every published stable version")


def asset_names(system, architecture):
    if system not in SYSTEMS or architecture not in ARCHITECTURES:
        raise ValueError("Unsupported release platform")
    if system == "linux":
        desktop = f"zed-linux-{architecture}.tar.gz"
    else:
        desktop = f"Zed-{architecture}.{'dmg' if system == 'macos' else 'exe'}"
    remote = f"zed-remote-server-{system}-{architecture}.{'zip' if system == 'windows' else 'gz'}"
    return desktop, remote


def expected_assets():
    return {name for system in SYSTEMS for architecture in ARCHITECTURES for name in asset_names(system, architecture)}


def check_signing():
    required = (
        "MACOS_CERTIFICATE", "MACOS_CERTIFICATE_PASSWORD", "MACOS_SIGNING_IDENTITY",
        "APPLE_NOTARIZATION_KEY", "APPLE_NOTARIZATION_KEY_ID", "APPLE_NOTARIZATION_ISSUER_ID",
        "AZURE_TENANT_ID", "AZURE_CLIENT_ID", "AZURE_CLIENT_SECRET",
        "ACCOUNT_NAME", "CERT_PROFILE_NAME", "ENDPOINT", "WINDOWS_SIGNING_PUBLISHER",
    )
    missing = [name for name in required if not os.environ.get(name, "").strip()]
    if missing:
        raise ValueError("Missing signing configuration: " + ", ".join(missing))
    if "Zed Industries" in os.environ["MACOS_SIGNING_IDENTITY"]:
        raise ValueError("Configure bex's own Developer ID signing identity")


def prepare(tag):
    version = validate_tag(tag)
    Path("crates/zed/RELEASE_CHANNEL").write_text("stable\n")
    environment = os.environ.get("GITHUB_ENV")
    if not environment:
        raise ValueError("prepare must run inside GitHub Actions")
    with Path(environment).open("a") as output:
        output.write(f"RELEASE_VERSION={version}\nZED_RELEASE_CHANNEL=stable\n")


def collect(system, architecture):
    destination = Path("release-artifacts")
    destination.mkdir(exist_ok=True)
    for index, name in enumerate(asset_names(system, architecture)):
        source = Path("target") / name
        if system == "linux" and index == 0:
            source = Path("target/release") / name
        if system == "macos" and index == 0:
            source = Path(f"target/{architecture}-apple-darwin/release") / name
            run("codesign", "--verify", "--verbose=2", str(source))
            run("xcrun", "stapler", "validate", str(source))
        if not source.is_file() or source.stat().st_size == 0:
            raise ValueError(f"Missing or empty release artifact: {source}")
        shutil.copyfile(source, destination / name)


def verify_artifacts(directory):
    files = {path.name for path in directory.iterdir() if path.is_file()}
    expected = expected_assets()
    if files != expected:
        raise ValueError(f"Release artifact mismatch: missing {sorted(expected - files)}, unexpected {sorted(files - expected)}")
    checksums = []
    for name in sorted(expected):
        path = directory / name
        if path.stat().st_size == 0:
            raise ValueError(f"Empty release artifact: {name}")
        with path.open("rb") as source:
            digest = hashlib.file_digest(source, "sha256").hexdigest()
        checksums.append(f"{digest}  {name}\n")
    return checksums


def publish(tag):
    validate_tag(tag)
    releases = release_list()
    validate_release(tag, releases)
    directory = Path("release-artifacts")
    checksums = verify_artifacts(directory)
    (directory / "SHA256SUMS").write_text("".join(checksums))
    if not any(release["tag_name"] == tag for release in releases):
        run("gh", "release", "create", tag, "--repo", REPOSITORY, "--verify-tag", "--draft", "--title", f"bex-desktop {tag[1:]}", "--generate-notes")
    run("gh", "release", "upload", tag, "--repo", REPOSITORY, "--clobber", *(str(path) for path in sorted(directory.iterdir())))
    release = json.loads(run("gh", "api", f"repos/{REPOSITORY}/releases/tags/{tag}"))
    if not release["draft"]:
        raise ValueError("Release became public during upload; refusing to modify it")
    expected = expected_assets() | {"SHA256SUMS"}
    uploaded = release["assets"]
    if len(uploaded) != len(expected) or {asset["name"] for asset in uploaded} != expected:
        raise ValueError("Uploaded release does not have the exact expected asset set")
    for asset in uploaded:
        if asset["state"] != "uploaded" or asset["size"] != (directory / asset["name"]).stat().st_size:
            raise ValueError(f"Incomplete uploaded artifact: {asset['name']}")
    # Recheck after the long build/upload window, before advancing the stable feed.
    validate_release(tag, release_list())
    run("gh", "release", "edit", tag, "--repo", REPOSITORY, "--draft=false", "--prerelease=false", "--latest")
    print(f"Published https://github.com/{REPOSITORY}/releases/tag/{tag}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("validate", "check-signing", "prepare", "collect", "publish"))
    parser.add_argument("system", nargs="?", choices=SYSTEMS)
    parser.add_argument("architecture", nargs="?", choices=ARCHITECTURES)
    arguments = parser.parse_args()
    tag = os.environ.get("BEX_RELEASE_TAG", "")
    if arguments.command == "validate":
        validate_tag(tag)
        validate_release(tag, release_list())
    elif arguments.command == "check-signing":
        check_signing()
    elif arguments.command == "prepare":
        prepare(tag)
    elif arguments.command == "collect":
        collect(arguments.system, arguments.architecture)
    else:
        publish(tag)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"Release failed: {error}", file=sys.stderr)
        sys.exit(1)
