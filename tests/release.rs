//! Release, packaging and deployment contract tests.
//!
//! These tests are deliberately static where the release artifact cannot be
//! built on the headless gate (no aarch64 cross toolchain, no Slint or
//! GStreamer development packages). They check the shipped templates, the
//! workflow policy, the deployment scripts' syntax and the rollback state
//! contract; they never claim that an aarch64 artifact or hardware acceptance
//! exists.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tv_client::config::{ClientConfig, YoutubeHwdec};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
const WORKFLOW: &str = ".github/workflows/release.yml";

fn repo_path(relative: &str) -> PathBuf {
    Path::new(ROOT).join(relative)
}

fn read(relative: &str) -> String {
    fs::read_to_string(repo_path(relative))
        .unwrap_or_else(|error| panic!("could not read {relative}: {error}"))
}

fn run_bash(script: &str, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(repo_path(script))
        .args(args)
        .current_dir(ROOT)
        .output()
        .unwrap_or_else(|error| panic!("could not run {script}: {error}"))
}

fn bash_syntax(script: &str) -> Output {
    Command::new("bash")
        .arg("-n")
        .arg(repo_path(script))
        .current_dir(ROOT)
        .output()
        .unwrap_or_else(|error| panic!("could not run bash -n {script}: {error}"))
}

fn describe(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn active_lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

fn has_active_key(text: &str, key: &str) -> bool {
    text.lines().map(str::trim).any(|line| {
        line.strip_prefix(key)
            .is_some_and(|rest| rest.starts_with(' ') || rest.starts_with('='))
    })
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|character| character.is_ascii_hexdigit())
}

#[test]
fn deploy_example_parses_with_youtube_playback_contract() {
    let raw = read("deploy/client.toml.example");
    let config = ClientConfig::from_toml_str(&raw).expect("deploy example must parse strictly");

    assert!(
        !config.youtube_playback_enabled,
        "the shipped kill switch must default to off"
    );
    assert_eq!(config.youtube_playback_timeout_secs, 60);
    assert_eq!(config.youtube_hwdec, YoutubeHwdec::AutoSafe);
    let ytdlp = config
        .youtube_ytdlp_path
        .as_deref()
        .expect("the example should name the managed extractor");
    assert!(
        ytdlp.is_absolute(),
        "yt-dlp path must be absolute: {ytdlp:?}"
    );
    assert_eq!(ytdlp.file_name(), Some(std::ffi::OsStr::new("yt-dlp")));
    assert_eq!(
        config.youtube_mpv_path.as_deref(),
        Some(Path::new("/usr/bin/mpv"))
    );
    assert!(!config.cloud_enabled);
    assert!(!config.youtube_enabled);

    // Parsing alone would pass if the YouTube keys were merely omitted and the
    // defaults applied, so require the actual keys in the template.
    for key in [
        "jellyfin_url",
        "server_url",
        "cloud_enabled",
        "youtube_enabled",
        "youtube_playback_enabled",
        "youtube_ytdlp_path",
        "youtube_mpv_path",
        "youtube_playback_timeout_secs",
        "youtube_hwdec",
        "airplay_enabled",
        "webrtc_enabled",
        "max_width",
        "max_height",
        "max_fps",
        "bitrate_bps",
        "max_bitrate_bps",
        "artwork_cache_bytes",
        "share_titles",
        "ui_safe_area_percent",
    ] {
        assert!(has_active_key(&raw, key), "example is missing key {key}");
    }

    assert_eq!(config.ui_safe_area_percent, 0);
    assert!(
        raw.contains("https://ovh-anders.minotaur-gacrux.ts.net"),
        "the example must use the staging control-server origin"
    );
    assert!(
        !raw.contains("ovh-1"),
        "the retired ovh-1 hostname must not reappear"
    );

    assert!(
        raw.contains("HDMI-CEC") && raw.contains("no configuration key"),
        "the example must explain that CEC is runtime-only, not a config key"
    );
    assert!(
        !raw.lines().any(|line| line.trim_start().starts_with("cec")),
        "ClientConfig rejects unknown keys, so the example must not add a cec key"
    );
}

#[test]
fn release_artifact_name_and_feature_policy_are_fixed() {
    let output = run_bash("scripts/package-release.sh", &["--print-name", "0.1.0"]);
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "tv-client-linux-arm64-0.1.0.tar.gz"
    );

    let output = run_bash(
        "scripts/package-release.sh",
        &["--print-name", "1.2.3-rc.1"],
    );
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "tv-client-linux-arm64-1.2.3-rc.1.tar.gz"
    );

    let workflow = read(WORKFLOW);
    for needle in [
        "runs-on: ubuntu-24.04-arm",
        "cargo build --release --locked --features native-ui,gstreamer-player",
        "scripts/package-release.sh",
        "SBOM.cdx.json",
        "NOTICES",
        "SHA256SUMS",
        "sha256sum -c SHA256SUMS",
        "ldd",
        "file \"$path\"",
    ] {
        assert!(workflow.contains(needle), "workflow is missing {needle:?}");
    }
    assert!(
        !workflow.contains("--all-features"),
        "release builds must use the explicit shipped feature set"
    );

    let cargo = read("Cargo.toml");
    assert!(cargo.contains("native-ui = [\"dep:slint\"]"));
    assert!(cargo.contains("gstreamer-player = ["));
    assert!(cargo.contains("name = \"tv-native-player\""));
    assert!(cargo.contains("required-features = [\"gstreamer-player\"]"));

    let package = read("scripts/package-release.sh");
    for needle in [
        "tv-client-linux-arm64",
        "tv-native-player",
        "NOTICES",
        "SBOM.cdx.json",
        "SHA256SUMS",
        "gzip -n",
        "CycloneDX",
        "--features native-ui,gstreamer-player",
    ] {
        assert!(
            package.contains(needle),
            "packaging script is missing {needle:?}"
        );
    }
}

#[test]
fn release_workflow_pins_actions_and_never_uses_secrets() {
    let workflow = read(WORKFLOW);

    let mut uses = 0;
    for line in workflow.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("uses:") else {
            continue;
        };
        uses += 1;
        // Action lines may carry an inline `# vX.Y.Z` comment; strip it before
        // validating the reference.
        let reference = rest
            .trim()
            .split(" #")
            .next()
            .expect("use reference")
            .trim();
        let (action, pin) = reference
            .rsplit_once('@')
            .unwrap_or_else(|| panic!("action is not pinned: {reference}"));
        assert!(!action.is_empty(), "empty action in {reference}");
        assert!(
            is_full_sha(pin),
            "action must be pinned to a full 40-character SHA, not a tag: {reference}"
        );
    }
    assert!(uses >= 4, "expected checkout, artifact and attest actions");

    assert!(
        !workflow.contains("secrets.") && !workflow.contains("${{ secrets"),
        "pull request builds must not consume repository secrets"
    );
    assert!(
        !workflow.contains("pull_request_target"),
        "fork pull requests must not run with a write-capable trigger"
    );
    assert!(
        !workflow.contains("contents: write"),
        "the release workflow does not need contents: write"
    );
    assert!(workflow.contains("permissions:\n  contents: read"));
    assert!(workflow.contains("id-token: write"));
    assert!(workflow.contains("attestations: write"));
    assert!(
        workflow.contains("if: startsWith(github.ref, 'refs/tags/v')"),
        "keyless attestation must be restricted to tag builds"
    );
    assert!(workflow.contains("actions/attest-build-provenance@"));
}

#[test]
fn deployment_scripts_are_syntax_clean_and_documented() {
    for script in [
        "scripts/deploy-common.sh",
        "scripts/install-release.sh",
        "scripts/rollback.sh",
        "scripts/package-release.sh",
    ] {
        let output = bash_syntax(script);
        assert!(
            output.status.success(),
            "bash -n rejected {script}: {}",
            describe(&output)
        );
        let metadata = fs::metadata(repo_path(script))
            .unwrap_or_else(|error| panic!("could not stat {script}: {error}"));
        assert_ne!(
            metadata.permissions().mode() & 0o111,
            0,
            "{script} must be executable"
        );
    }

    for script in ["scripts/install-release.sh", "scripts/rollback.sh"] {
        let output = run_bash(script, &["--help"]);
        assert!(output.status.success(), "{}", describe(&output));
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains("--check"),
            "{script} --help must document --check"
        );
        assert!(
            text.contains("--dry-run"),
            "{script} --help must document --dry-run"
        );
    }

    // Deployment helpers must not install packages or rewrite the firewall as
    // a side effect of an application update, and must not invoke sudo (the
    // kiosk account is unprivileged; the installer runs as root).
    for script in [
        "scripts/install-release.sh",
        "scripts/rollback.sh",
        "scripts/deploy-common.sh",
        "deploy/kiosk-profile.sh.example",
    ] {
        for line in active_lines(&read(script)) {
            for forbidden in [
                "apt-get",
                "apt install",
                "iptables",
                "nft ",
                "ufw ",
                "firewall-cmd",
            ] {
                assert!(
                    !line.contains(forbidden),
                    "{script} must not run {forbidden:?}: {line}"
                );
            }
            let invokes_sudo = line.starts_with("sudo ")
                || line.starts_with("sudo\t")
                || line.contains("$(sudo ")
                || line.contains("| sudo ");
            assert!(!invokes_sudo, "{script} must not invoke sudo: {line}");
        }
    }
}

#[test]
fn kiosk_session_uses_cage_gles_and_an_unprivileged_user() {
    let profile = read("deploy/kiosk-profile.sh.example");
    for needle in [
        "WLR_RENDERER=gles2",
        "unset WLR_BACKENDS",
        "SLINT_BACKEND=winit-femtovg",
        "cage -- /opt/tv-client/current/tv-client",
        "/var/lib/tv-client/client.toml",
        "/etc/tv-client/client.toml",
        r#"--config "$kiosk_config""#,
        "--state-dir /var/lib/tv-client",
        "/dev/tty1",
        "SSH_CONNECTION",
        "backoff",
        r#"sleep "$backoff""#,
    ] {
        assert!(
            profile.contains(needle),
            "kiosk profile is missing {needle:?}"
        );
    }
    assert!(
        !profile.contains("winit-software"),
        "Cage needs the Wayland/FemtoVG backend, not the software backend"
    );
    assert!(
        !profile.contains("sudo"),
        "the kiosk session must never run through sudo"
    );

    let getty = read("deploy/getty-override.conf.example");
    assert!(getty.contains("--autologin tvkiosk"));
    assert!(!getty.contains("--autologin root"));
    assert!(getty.contains("Restart=always"));
    assert!(getty.contains("RestartSec="));
    assert!(getty.contains("getty@tty1"));

    let install = read("scripts/install-release.sh");
    for needle in [
        "--shell /bin/bash",
        "passwd -l",
        "authorized_keys",
        "getty@tty2.service",
        "mv -T",
        "/var/lib/tv-client",
        "/etc/tv-client",
        "TV_STATE_CONFIG",
        "install_state_config",
        "lightdm",
        "gdm3",
        "sddm",
        "systemctl disable --now",
        "chvt 1",
        "--keep-display-manager",
    ] {
        assert!(install.contains(needle), "installer is missing {needle:?}");
    }
    assert!(
        !install.contains("--groups sudo") && !install.contains("--groups=sudo"),
        "the kiosk account must never be created in a privileged group"
    );
}

#[test]
fn install_dry_run_creates_state_directory_under_staging_prefix() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let build = temporary.path().join("build");
    fs::create_dir_all(&build).expect("build directory");
    for binary in ["tv-client", "tv-native-player"] {
        let path = build.join(binary);
        fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write binary");
        let mut permissions = fs::metadata(&path).expect("stat binary").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod binary");
    }
    fs::write(build.join("NOTICES"), "test notices\n").expect("write notices");
    fs::write(build.join("SBOM.cdx.json"), "{}\n").expect("write SBOM");

    let artifact = temporary.path().join("tv-client-linux-arm64-0.1.0.tar.gz");
    let status = Command::new("tar")
        .args([
            "-czf",
            artifact.to_str().expect("artifact path"),
            "-C",
            build.to_str().expect("build path"),
            "tv-client",
            "tv-native-player",
            "NOTICES",
            "SBOM.cdx.json",
        ])
        .status()
        .expect("run tar");
    assert!(status.success(), "tar failed");

    let mut hash_command = Command::new("sha256sum");
    hash_command.arg(&artifact);
    let hash_output = hash_command.output().expect("run sha256sum");
    assert!(hash_output.status.success(), "sha256sum failed");
    let hash_text = String::from_utf8_lossy(&hash_output.stdout);
    let hash = hash_text.split_whitespace().next().expect("sha256 output");
    let artifact_name = artifact
        .file_name()
        .expect("artifact name")
        .to_string_lossy();
    fs::write(
        temporary.path().join("SHA256SUMS"),
        format!("{hash}  {artifact_name}\n"),
    )
    .expect("write checksums");

    let prefix = temporary.path().join("stage");
    let output = run_bash(
        "scripts/install-release.sh",
        &[
            "--dry-run",
            "--allow-foreign-arch",
            "--allow-unsupported-os",
            "--prefix",
            prefix.to_str().expect("prefix path"),
            "--artifact",
            artifact.to_str().expect("artifact path"),
        ],
    );
    assert!(output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    let state_command = "install -d -m 0700 -o tvkiosk -g tvkiosk";
    assert!(stderr.contains("would create state directory"), "{stderr}");
    assert!(stderr.contains(state_command), "{stderr}");
    assert!(
        stderr.contains(
            prefix
                .join("var/lib/tv-client")
                .to_str()
                .expect("state path")
        ),
        "{stderr}"
    );
    assert!(!prefix.exists(), "dry-run must not create files");
}

fn build_fake_artifact(directory: &Path) -> PathBuf {
    let build = directory.join("build");
    fs::create_dir_all(&build).expect("build directory");
    for binary in ["tv-client", "tv-native-player"] {
        let path = build.join(binary);
        fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write binary");
        let mut permissions = fs::metadata(&path).expect("stat binary").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod binary");
    }
    fs::write(build.join("NOTICES"), "test notices\n").expect("write notices");
    fs::write(build.join("SBOM.cdx.json"), "{}\n").expect("write SBOM");

    let artifact = directory.join("tv-client-linux-arm64-0.1.1.tar.gz");
    let status = Command::new("tar")
        .args([
            "-czf",
            artifact.to_str().expect("artifact path"),
            "-C",
            build.to_str().expect("build path"),
            "tv-client",
            "tv-native-player",
            "NOTICES",
            "SBOM.cdx.json",
        ])
        .status()
        .expect("run tar");
    assert!(status.success(), "tar failed");

    let hash_output = Command::new("sha256sum")
        .arg(&artifact)
        .output()
        .expect("run sha256sum");
    assert!(hash_output.status.success(), "sha256sum failed");
    let hash_text = String::from_utf8_lossy(&hash_output.stdout);
    let hash = hash_text.split_whitespace().next().expect("sha256 output");
    let artifact_name = artifact
        .file_name()
        .expect("artifact name")
        .to_string_lossy();
    fs::write(
        directory.join("SHA256SUMS"),
        format!("{hash}  {artifact_name}\n"),
    )
    .expect("write checksums");
    artifact
}

fn run_staged_install(prefix: &Path, artifact: &Path) -> Output {
    run_bash(
        "scripts/install-release.sh",
        &[
            "--staging-install",
            prefix.to_str().expect("prefix path"),
            "--allow-foreign-arch",
            "--allow-unsupported-os",
            "--artifact",
            artifact.to_str().expect("artifact path"),
        ],
    )
}

fn run_staged_dry_run(prefix: &Path, artifact: &Path) -> Output {
    run_bash(
        "scripts/install-release.sh",
        &[
            "--dry-run",
            "--allow-foreign-arch",
            "--allow-unsupported-os",
            "--prefix",
            prefix.to_str().expect("prefix path"),
            "--artifact",
            artifact.to_str().expect("artifact path"),
        ],
    )
}

fn write_config(path: &Path, contents: &[u8]) {
    fs::create_dir_all(path.parent().expect("config parent")).expect("config directory");
    fs::write(path, contents).expect("write config");
    let mut permissions = fs::metadata(path).expect("stat config").permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(path, permissions).expect("chmod config");
}

fn file_mode(path: &Path) -> u32 {
    fs::metadata(path)
        .unwrap_or_else(|error| panic!("could not stat {}: {error}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

fn id_value(flag: &str) -> u32 {
    let output = Command::new("id")
        .arg(flag)
        .output()
        .unwrap_or_else(|error| panic!("could not run id {flag}: {error}"));
    assert!(output.status.success(), "id {flag} failed");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|error| panic!("id {flag} did not print a number: {error}"))
}

#[test]
fn install_seeds_state_config_from_existing_operator_config() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let artifact = build_fake_artifact(temporary.path());
    let prefix = temporary.path().join("stage");
    let operator_path = prefix.join("etc/tv-client/client.toml");
    let operator_config =
        b"# operator-secret-sentinel\njellyfin_url = \"http://jellyfin.local\"\nserver_url = \"https://control.example\"\ncloud_enabled = false\n"
            .to_vec();
    write_config(&operator_path, &operator_config);

    let output = run_staged_install(&prefix, &artifact);
    assert!(output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);

    let state_path = prefix.join("var/lib/tv-client/client.toml");
    assert_eq!(
        fs::read(&state_path).expect("read state config"),
        operator_config,
        "the upgrade must seed the state copy from the existing operator config"
    );
    assert_eq!(file_mode(&state_path), 0o600, "state config must be 0600");
    let state_metadata = fs::metadata(&state_path).expect("stat state config");
    assert_eq!(
        state_metadata.uid(),
        id_value("-u"),
        "state config must be owned by the kiosk-staging user"
    );
    assert_eq!(
        state_metadata.gid(),
        id_value("-g"),
        "state config must use the kiosk-staging group"
    );
    assert_eq!(
        file_mode(&prefix.join("var/lib/tv-client")),
        0o700,
        "state directory must be 0700"
    );
    assert_eq!(
        fs::read(&operator_path).expect("read operator config"),
        operator_config,
        "the operator config must not be rewritten"
    );
    assert_eq!(file_mode(&operator_path), 0o644);
    assert!(
        stderr.contains("seeded") && stderr.contains("from the operator config"),
        "the install must report the operator source: {stderr}"
    );
    assert!(
        !stderr.contains("client.toml.example"),
        "an upgrade must not seed from the placeholder example: {stderr}"
    );
    assert!(
        !stderr.contains("operator-secret-sentinel"),
        "the installer must never log configuration contents: {stderr}"
    );
    assert!(
        prefix.join("releases/0.1.1/tv-client").is_file(),
        "the staged install must extract the release"
    );
    assert_eq!(
        fs::read_link(prefix.join("current")).expect("current symlink"),
        prefix.join("releases/0.1.1")
    );

    // A rerun must keep the seeded file even when /etc changes afterwards.
    let changed = b"server_url = \"https://changed.example\"\n";
    write_config(&operator_path, changed);
    let output = run_staged_install(&prefix, &artifact);
    assert!(output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        fs::read(&state_path).expect("read state config"),
        operator_config,
        "a rerun must not overwrite the existing state config"
    );
    assert!(
        stderr.contains("keeping existing")
            && stderr.contains(state_path.to_str().expect("state path")),
        "the rerun must report that the state config is kept: {stderr}"
    );
    assert!(
        !stderr.contains("operator-secret-sentinel"),
        "the rerun must not log configuration contents: {stderr}"
    );
}

#[test]
fn install_seeds_state_config_from_example_without_operator_config() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let artifact = build_fake_artifact(temporary.path());
    let prefix = temporary.path().join("stage");
    let example = fs::read(repo_path("deploy/client.toml.example")).expect("read example");

    let output = run_staged_install(&prefix, &artifact);
    assert!(output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);

    let state_path = prefix.join("var/lib/tv-client/client.toml");
    assert_eq!(
        fs::read(&state_path).expect("read state config"),
        example,
        "a fresh install must seed the state copy from the example"
    );
    assert_eq!(file_mode(&state_path), 0o600, "state config must be 0600");
    let state_metadata = fs::metadata(&state_path).expect("stat state config");
    assert_eq!(state_metadata.uid(), id_value("-u"));
    assert_eq!(state_metadata.gid(), id_value("-g"));
    assert_eq!(
        fs::read(prefix.join("etc/tv-client/client.toml")).expect("read operator config"),
        example,
        "the operator config must be created from the same example"
    );
    assert_eq!(file_mode(&prefix.join("etc/tv-client/client.toml")), 0o644);
    assert!(
        stderr.contains("client.toml.example"),
        "the fallback seed must name the example source: {stderr}"
    );
}

#[test]
fn install_dry_run_reports_operator_config_seed_source_without_mutation() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let artifact = build_fake_artifact(temporary.path());
    let prefix = temporary.path().join("stage");
    let operator_path = prefix.join("etc/tv-client/client.toml");
    let operator_config = b"# existing operator config\ncloud_enabled = false\n".to_vec();
    write_config(&operator_path, &operator_config);

    let output = run_staged_dry_run(&prefix, &artifact);
    assert!(output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("would seed")
            && stderr.contains(operator_path.to_str().expect("operator path")),
        "dry-run must report the existing operator config as the seed source: {stderr}"
    );
    assert!(
        !stderr.contains("client.toml.example"),
        "dry-run must not plan the example when the operator config exists: {stderr}"
    );
    assert_eq!(
        fs::read(&operator_path).expect("read operator config"),
        operator_config,
        "dry-run must not modify the operator config"
    );
    assert!(
        !prefix.join("var/lib/tv-client").exists(),
        "dry-run must not create the state directory"
    );
    assert!(
        !prefix.join("releases").exists(),
        "dry-run must not install a release"
    );
}

#[test]
fn installer_display_manager_handling_is_dry_run_safe_and_opt_out() {
    let help = run_bash("scripts/install-release.sh", &["--help"]);
    assert!(help.status.success(), "{}", describe(&help));
    let help_text = format!(
        "{}{}",
        String::from_utf8_lossy(&help.stdout),
        String::from_utf8_lossy(&help.stderr)
    );
    assert!(
        help_text.contains("--keep-display-manager"),
        "installer --help must document the display-manager opt-out: {help_text}"
    );

    let temporary = tempfile::tempdir().expect("temporary directory");
    let artifact = build_fake_artifact(temporary.path());
    let prefix = temporary.path().join("stage");
    fs::create_dir_all(prefix.join("etc/systemd/system")).expect("systemd staging directory");
    fs::write(
        prefix.join("etc/systemd/system/lightdm.service"),
        "[Unit]\nDescription=test display manager\n",
    )
    .expect("write fake lightdm unit");

    let output = run_bash(
        "scripts/install-release.sh",
        &[
            "--dry-run",
            "--allow-foreign-arch",
            "--allow-unsupported-os",
            "--prefix",
            prefix.to_str().expect("prefix path"),
            "--artifact",
            artifact.to_str().expect("artifact path"),
        ],
    );
    assert!(output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("+ systemctl disable --now lightdm"),
        "dry-run must print the disable plan: {stderr}"
    );
    assert!(
        stderr.contains("+ chvt 1") || stderr.contains("chvt is unavailable"),
        "dry-run must plan tty1 activation or report it unavailable: {stderr}"
    );
    assert!(
        stderr.contains("would seed")
            && stderr.contains("client.toml")
            && stderr.contains("client.toml.example")
            && stderr.contains("0600"),
        "dry-run must plan the state configuration seed from the example: {stderr}"
    );
    assert!(
        !prefix.join("var/lib/tv-client").exists(),
        "dry-run must not create the state directory"
    );
    assert!(
        !prefix.join("opt/tv-client/releases").exists(),
        "dry-run must not create a release"
    );

    let output = run_bash(
        "scripts/install-release.sh",
        &[
            "--dry-run",
            "--keep-display-manager",
            "--allow-foreign-arch",
            "--allow-unsupported-os",
            "--prefix",
            prefix.to_str().expect("prefix path"),
            "--artifact",
            artifact.to_str().expect("artifact path"),
        ],
    );
    assert!(output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("systemctl disable --now lightdm"),
        "--keep-display-manager must not disable lightdm: {stderr}"
    );
    assert!(
        stderr.contains("--keep-display-manager"),
        "the opt-out must be logged: {stderr}"
    );
}

fn set_directory_mtime(path: &Path, stamp: &str) {
    let mut command = Command::new("touch");
    command.arg("-d").arg(stamp).arg(path);
    let status = command.status().expect("run touch");
    assert!(status.success(), "touch failed for {stamp}");
}

fn write_fake_release(releases: &Path, version: &str) {
    let directory = releases.join(version);
    fs::create_dir_all(&directory).expect("create fake release");
    for binary in ["tv-client", "tv-native-player"] {
        let path = directory.join(binary);
        fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write fake binary");
        let mut permissions = fs::metadata(&path).expect("stat fake binary").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod fake binary");
    }
}

#[test]
fn rollback_selects_previous_release_and_preserves_state() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let prefix = temporary.path().join("opt");
    let releases = prefix.join("releases");
    write_fake_release(&releases, "0.9.0");
    set_directory_mtime(&releases.join("0.9.0"), "2024-01-01 00:00:00");
    write_fake_release(&releases, "1.0.0");
    set_directory_mtime(&releases.join("1.0.0"), "2024-01-02 00:00:00");
    std::os::unix::fs::symlink(releases.join("1.0.0"), prefix.join("current"))
        .expect("create current symlink");

    let prefix_argument = prefix.to_str().expect("utf-8 temporary path");

    let output = run_bash(
        "scripts/rollback.sh",
        &["--check", "--prefix", prefix_argument],
    );
    assert!(output.status.success(), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("rollback target: 0.9.0"),
        "{}",
        describe(&output)
    );
    let current_after_check = fs::read_link(prefix.join("current")).expect("current symlink");
    assert_eq!(
        current_after_check,
        releases.join("1.0.0"),
        "--check must not change the current release"
    );

    let output = run_bash(
        "scripts/rollback.sh",
        &["--dry-run", "--prefix", prefix_argument],
    );
    assert!(output.status.success(), "{}", describe(&output));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("ln -s"), "{}", describe(&output));
    assert!(text.contains("mv -T"), "{}", describe(&output));
    let current_after_dry_run = fs::read_link(prefix.join("current")).expect("current symlink");
    assert_eq!(
        current_after_dry_run,
        releases.join("1.0.0"),
        "--dry-run must not change the current release"
    );
    assert!(
        !fs::read_dir(&prefix)
            .expect("prefix listing")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".current.")),
        "dry-run must not leave a temporary symlink"
    );

    let output = run_bash(
        "scripts/rollback.sh",
        &["--list", "--prefix", prefix_argument],
    );
    assert!(output.status.success(), "{}", describe(&output));
    let listing = String::from_utf8_lossy(&output.stdout);
    assert!(listing.contains("0.9.0"));
    assert!(listing.contains("1.0.0 (current)"));
    assert!(!listing.contains("0.8.0"));

    let output = run_bash(
        "scripts/rollback.sh",
        &["--to", "0.9.0", "--prefix", prefix_argument],
    );
    assert!(
        !output.status.success(),
        "a custom prefix must not be used for a real rollback"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--check or --dry-run"),
        "{}",
        describe(&output)
    );

    write_fake_release(&releases, "0.8.0");
    fs::remove_file(releases.join("0.8.0/tv-native-player")).expect("remove fake binary");
    let output = run_bash(
        "scripts/rollback.sh",
        &["--to", "0.8.0", "--check", "--prefix", prefix_argument],
    );
    assert!(
        !output.status.success(),
        "an incomplete release must be refused"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("incomplete"),
        "{}",
        describe(&output)
    );

    let rollback = read("scripts/rollback.sh");
    let active = active_lines(&rollback);
    assert!(active.iter().any(|line| line.contains("mv -T")));
    assert!(
        !active
            .iter()
            .any(|line| line.contains("rm -rf") && !line.contains("temporary_link")),
        "rollback must not remove an installed release"
    );
    for line in active {
        if line.contains("TV_STATE_DIR") || line.contains("TV_CONFIG_DIR") {
            assert!(
                line.trim_start().starts_with("log "),
                "rollback may only mention persistent state in a log line: {line}"
            );
        }
    }
}

#[test]
fn rollback_uses_install_order_for_prerelease_versions() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let prefix = temporary.path().join("opt");
    let releases = prefix.join("releases");
    write_fake_release(&releases, "1.0.0-rc.1");
    set_directory_mtime(&releases.join("1.0.0-rc.1"), "2024-01-01 00:00:00");
    write_fake_release(&releases, "1.0.0");
    set_directory_mtime(&releases.join("1.0.0"), "2024-01-02 00:00:00");
    std::os::unix::fs::symlink(releases.join("1.0.0"), prefix.join("current"))
        .expect("create current symlink");

    let output = run_bash(
        "scripts/rollback.sh",
        &[
            "--check",
            "--prefix",
            prefix.to_str().expect("temporary path"),
        ],
    );
    assert!(output.status.success(), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("rollback target: 1.0.0-rc.1"),
        "{}",
        describe(&output)
    );

    let rollback = read("scripts/rollback.sh");
    assert!(rollback.contains("-printf '%T@"));
    assert!(!rollback.contains("sort -V"));
}

#[test]
fn release_docs_report_unrun_gates_honestly() {
    let release = read("docs/release.md");
    for needle in [
        "Core local",
        "Pairing",
        "YouTube",
        "Reboot",
        "power loss",
        "Offline",
        "relay",
        "soak",
        "Rollback",
        "NOT RUN",
        "NOT BUILT",
        "720p30",
        "1080p",
        "unsupported",
        "SHA256SUMS",
        "attestation",
        "scripts/install-release.sh",
        "scripts/rollback.sh",
    ] {
        assert!(
            release.contains(needle),
            "release docs are missing {needle:?}"
        );
    }
    assert!(
        release.matches("NOT RUN").count() >= 10,
        "every unexecuted matrix row must be marked NOT RUN"
    );
    assert!(
        !release.contains("| PASS |"),
        "release docs must not claim an unrun gate"
    );
    assert!(release.contains("/var/lib/tv-client/client.db"));
    assert!(!release.contains("/var/lib/tv-client/state.db"));
    assert!(release.contains("--prefix /var/tmp/tv-client-stage"));
    assert!(release.contains("`--allow-foreign-arch` is rejected for a real install"));
    assert!(release.contains("`--allow-unsupported-os`"));

    let build = read("docs/build.md");
    for section in [
        "## Headless gate",
        "## Platform features",
        "## CI",
        "## Raspberry Pi target (aarch64)",
        "ARM64 release build",
        ".github/workflows/release.yml",
        "ubuntu-24.04-arm",
    ] {
        assert!(
            build.contains(section),
            "build docs lost or lack {section:?}"
        );
    }
}

#[test]
fn release_inventory_is_cyclonedx_and_lists_licenses() {
    let jq_available = Command::new("jq")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !jq_available {
        eprintln!("jq is not installed; skipping the release inventory conversion test");
        return;
    }

    let temporary = tempfile::tempdir().expect("temporary directory");
    let metadata = temporary.path().join("metadata.json");
    let output_dir = temporary.path().join("inventory");
    let document = serde_json::json!({
        "packages": [
            {
                "id": "path+file:///work#tv-client@0.1.0",
                "name": "tv-client",
                "version": "0.1.0",
                "license": "GPL-3.0-only"
            },
            {
                "id": "registry+https://example.invalid#serde@1.0.0",
                "name": "serde",
                "version": "1.0.0",
                "license": "MIT OR Apache-2.0"
            }
        ],
        "resolve": {
            "root": "path+file:///work#tv-client@0.1.0",
            "nodes": [
                {
                    "id": "path+file:///work#tv-client@0.1.0",
                    "deps": [{
                        "name": "serde",
                        "pkg": "registry+https://example.invalid#serde@1.0.0",
                        "dep_kinds": [{"kind": null, "target": null}]
                    }]
                },
                {
                    "id": "registry+https://example.invalid#serde@1.0.0",
                    "deps": []
                }
            ]
        }
    });
    fs::write(
        &metadata,
        serde_json::to_vec_pretty(&document).expect("serialize fixture"),
    )
    .expect("write fixture");

    let output = Command::new("bash")
        .arg(repo_path("scripts/package-release.sh"))
        .arg("--inventory")
        .arg(&metadata)
        .arg(&output_dir)
        .current_dir(ROOT)
        .output()
        .expect("run inventory generation");
    assert!(output.status.success(), "{}", describe(&output));

    let sbom_text = fs::read_to_string(output_dir.join("SBOM.cdx.json")).expect("read SBOM");
    let sbom: serde_json::Value =
        serde_json::from_str(&sbom_text).expect("SBOM must be valid JSON");
    assert_eq!(sbom["bomFormat"], "CycloneDX");
    assert_eq!(sbom["specVersion"], "1.5");
    assert_eq!(sbom["metadata"]["component"]["name"], "tv-client");
    assert_eq!(
        sbom["metadata"]["component"]["purl"],
        "pkg:cargo/tv-client@0.1.0"
    );
    let components = sbom["components"].as_array().expect("components array");
    assert_eq!(components.len(), 1, "the root package is not a component");
    assert_eq!(components[0]["name"], "serde");
    assert_eq!(components[0]["purl"], "pkg:cargo/serde@1.0.0");
    assert_eq!(
        components[0]["licenses"][0]["expression"],
        "MIT OR Apache-2.0"
    );
    assert_eq!(
        sbom["dependencies"][0]["dependsOn"][0],
        "pkg:cargo/serde@1.0.0"
    );

    let notices = fs::read_to_string(output_dir.join("NOTICES")).expect("read notices");
    assert!(notices.contains("GPL-3.0-only"));
    assert!(notices.contains("serde 1.0.0"));
}

#[test]
fn native_player_hardware_path_is_configured_and_documented_as_unrun() {
    let profile = read("deploy/kiosk-profile.sh.example");
    for needle in ["TV_AUDIO_SINK=alsa", "TV_AUDIO_DEVICE=hw:0,0"] {
        assert!(
            profile.contains(needle),
            "kiosk profile is missing {needle:?}"
        );
    }

    let install = read("scripts/install-release.sh");
    assert!(
        install.contains("for group in input video render audio"),
        "the installer must grant the least-privilege input group"
    );

    let build = read("docs/build.md");
    for needle in [
        "TV_AUDIO_SINK",
        "TV_AUDIO_DEVICE",
        "TV_PLAYER_OVERLAY",
        "input",
        "NOT RUN",
    ] {
        assert!(build.contains(needle), "build docs are missing {needle:?}");
    }

    let release = read("docs/release.md");
    for needle in [
        "Native player controls and HDMI audio",
        "TV_AUDIO_SINK=alsa",
        "TV_AUDIO_DEVICE=hw:0,0",
        "`input` group",
        "textoverlay",
        "NOT RUN",
    ] {
        assert!(
            release.contains(needle),
            "release docs are missing {needle:?}"
        );
    }
}
