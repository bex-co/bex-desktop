use gh_workflow::*;

use super::{runners::Platform, vars::bundle_envs};

fn checkout() -> Step<Use> {
    Step::new("Action")
        .uses("actions", "checkout", "v4")
        .add_with(("fetch-depth", 0))
        .add_with(("persist-credentials", false))
}

fn python() -> Step<Use> {
    Step::new("Action")
        .uses("actions", "setup-python", "v5")
        .add_with(("python-version", "3.12"))
}

fn command(name: &str, script: &str) -> Step<Run> {
    Step::new(name).run(script).shell("bash")
}

pub fn bex_release() -> Workflow {
    let signing = bundle_envs(Platform::Mac)
        .add(
            "MACOS_SIGNING_IDENTITY",
            "${{ vars.MACOS_SIGNING_IDENTITY }}",
        )
        .add("AZURE_TENANT_ID", "${{ secrets.AZURE_SIGNING_TENANT_ID }}")
        .add("AZURE_CLIENT_ID", "${{ secrets.AZURE_SIGNING_CLIENT_ID }}")
        .add(
            "AZURE_CLIENT_SECRET",
            "${{ secrets.AZURE_SIGNING_CLIENT_SECRET }}",
        )
        .add("ACCOUNT_NAME", "${{ vars.AZURE_SIGNING_ACCOUNT_NAME }}")
        .add(
            "CERT_PROFILE_NAME",
            "${{ vars.AZURE_SIGNING_CERT_PROFILE_NAME }}",
        )
        .add("ENDPOINT", "${{ vars.AZURE_SIGNING_ENDPOINT }}")
        .add(
            "WINDOWS_SIGNING_PUBLISHER",
            "${{ vars.WINDOWS_SIGNING_PUBLISHER }}",
        );
    let mut workflow = Workflow::new("bex_release")
        .on(Event::default()
            .push(Push::default().add_tag("v*").add_branch("main").add_path("script/bex-release.py").add_path("script/test_bex_release.py").add_path("tooling/xtask/src/tasks/workflows/bex_release.rs").add_path(".github/workflows/bex_release.yml"))
            .workflow_dispatch(WorkflowDispatch::default()))
        .permissions(Permissions::default().contents(Level::Read))
        .concurrency(Concurrency::new(Expression::new("bex-stable-release")).cancel_in_progress(false))
        .add_env(("CARGO_INCREMENTAL", "0"))
        .add_env(("BEX_RELEASE_TAG", "${{ github.ref_name }}"))
        .add_job("checks", Job::new("Test release automation")
            .runs_on("ubuntu-24.04")
            .timeout_minutes(20u32)
            .add_step(checkout())
            .add_step(python())
            .add_step(command("Test publication guards", "python -m unittest discover -s script -p test_bex_release.py"))
            .add_step(command("Verify generated workflows", "cargo xtask workflows\ngit diff --exit-code -- .github/workflows extensions/workflows\ncargo xtask check-workflows")))
        .add_job("validate", Job::new("Validate release and signing configuration").add_need("checks")
            .runs_on("ubuntu-24.04")
            .cond(Expression::new("github.repository == 'bex-co/bex-desktop' && startsWith(github.ref, 'refs/tags/v')"))
            .timeout_minutes(10u32)
            .add_step(checkout())
            .add_step(python())
            .add_step(command("Validate tag and version", "python script/bex-release.py validate")
                .add_env(("GH_TOKEN", "${{ github.token }}")))
            .add_step(command("Check signing credentials", "python script/bex-release.py check-signing")
                .envs(signing)));
    let mut builds = Vec::new();
    for (os, arch, runner, platform) in [
        ("macos", "aarch64", "macos-15", Platform::Mac),
        ("macos", "x86_64", "macos-15-intel", Platform::Mac),
        ("linux", "aarch64", "ubuntu-22.04-arm", Platform::Linux),
        ("linux", "x86_64", "ubuntu-22.04", Platform::Linux),
        ("windows", "aarch64", "windows-2022", Platform::Windows),
        ("windows", "x86_64", "windows-2022", Platform::Windows),
    ] {
        let name = format!("build_{os}_{arch}");
        let mut job = Job::new(&name)
            .runs_on(runner)
            .add_need("validate")
            .timeout_minutes(180u32)
            .envs(
                bundle_envs(platform)
                    .add(
                        "WINDOWS_SIGNING_PUBLISHER",
                        "${{ vars.WINDOWS_SIGNING_PUBLISHER }}",
                    )
                    .add(
                        "MACOS_SIGNING_IDENTITY",
                        "${{ vars.MACOS_SIGNING_IDENTITY }}",
                    ),
            )
            .add_step(checkout())
            .add_step(python())
            .add_step(command(
                "Prepare stable build",
                "python script/bex-release.py prepare",
            ))
            .add_step(
                Step::new("Action")
                    .uses("actions", "setup-node", "v4")
                    .add_with(("node-version", "22")),
            );
        if os == "linux" {
            job = job
                .add_step(command(
                    "Install Linux dependencies",
                    "sudo apt-get update\nscript/linux",
                ))
                .add_step(command("Bundle Linux", "script/bundle-linux"));
        } else if os == "macos" {
            job = job.add_step(command(
                "Bundle and notarize macOS",
                &format!("script/bundle-mac {arch}-apple-darwin"),
            ));
        } else {
            job = job.add_step(
                Step::new("Install signing tools and bundle Windows")
                    .run(format!(
                        r#"$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
Install-Module -Name TrustedSigning -RequiredVersion 0.5.8 -Scope CurrentUser -Force -Repository PSGallery
Import-Module TrustedSigning
./script/bundle-windows.ps1 -Architecture {arch}
$signature = Get-AuthenticodeSignature "target/Zed-{arch}.exe"
if ($signature.Status -ne 'Valid') {{ throw 'Installer signature validation failed' }}"#
                    ))
                    .shell("pwsh"),
            );
        }
        job = job
            .add_step(command(
                "Collect release artifacts",
                &format!("python script/bex-release.py collect {os} {arch}"),
            ))
            .add_step(
                Step::new("Action")
                    .uses("actions", "upload-artifact", "v4")
                    .add_with(("name", format!("bex-{os}-{arch}")))
                    .add_with(("path", "release-artifacts/*"))
                    .add_with(("if-no-files-found", "error"))
                    .add_with(("compression-level", 0)),
            );
        workflow = workflow.add_job(&name, job);
        builds.push(name);
    }
    workflow.add_job(
        "publish",
        Job::new("Publish complete stable release")
            .runs_on("ubuntu-24.04")
            .needs(builds)
            .permissions(Permissions::default().contents(Level::Write))
            .timeout_minutes(15u32)
            .add_step(checkout())
            .add_step(python())
            .add_step(
                Step::new("Action")
                    .uses("actions", "download-artifact", "v4")
                    .add_with(("pattern", "bex-*"))
                    .add_with(("merge-multiple", true))
                    .add_with(("path", "release-artifacts")),
            )
            .add_step(
                command(
                    "Verify artifacts and publish",
                    "python script/bex-release.py publish",
                )
                .add_env(("GH_TOKEN", "${{ github.token }}")),
            ),
    )
}
