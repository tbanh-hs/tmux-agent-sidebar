use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::process::Stdio;
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::time::now_epoch_secs;
use crate::tmux;

pub(crate) const DESKTOP_NOTIFICATION_COOLDOWN_SECS: u64 = 120;
const DESKTOP_NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(3);
const DESKTOP_NOTIFICATION_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DesktopNotificationKind {
    TaskCompleted,
    TaskFailed,
    PermissionRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DesktopNotificationEvent {
    Stop,
    Notification,
    TaskCompleted,
    StopFailure,
    PermissionDenied,
}

impl DesktopNotificationEvent {
    pub const ALL: [Self; 5] = [
        Self::Stop,
        Self::Notification,
        Self::TaskCompleted,
        Self::StopFailure,
        Self::PermissionDenied,
    ];

    pub const DEFAULT: [Self; 4] = [
        Self::Stop,
        Self::Notification,
        Self::StopFailure,
        Self::PermissionDenied,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Notification => "notification",
            Self::TaskCompleted => "task_completed",
            Self::StopFailure => "stop_failure",
            Self::PermissionDenied => "permission_denied",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "stop" => Some(Self::Stop),
            "notification" => Some(Self::Notification),
            "task_completed" => Some(Self::TaskCompleted),
            "stop_failure" => Some(Self::StopFailure),
            "permission_denied" => Some(Self::PermissionDenied),
            _ => None,
        }
    }
}

/// Default sound name used when the sound option is enabled without an
/// explicit name (`@sidebar_notification_sound on`). macOS resolves this
/// against /System/Library/Sounds; other platforms treat it as a freedesktop
/// event id for a best-effort player.
#[cfg(target_os = "macos")]
const DEFAULT_SOUND_NAME: &str = "Ping";
#[cfg(not(target_os = "macos"))]
const DEFAULT_SOUND_NAME: &str = "message";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopNotificationSettings {
    pub enabled: bool,
    pub events: HashSet<DesktopNotificationEvent>,
    /// `Some(name)` plays a sound alongside the notification; `None` is silent.
    /// On macOS `name` is a system sound name; elsewhere it's a best-effort id.
    pub sound: Option<String>,
}

impl Default for DesktopNotificationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            events: DesktopNotificationEvent::DEFAULT.iter().copied().collect(),
            sound: None,
        }
    }
}

impl DesktopNotificationSettings {
    pub fn from_tmux_options(opts: &HashMap<String, String>) -> Self {
        Self::from_tmux_options_with_backend(opts, notification_backend_available())
    }

    fn from_tmux_options_with_backend(
        opts: &HashMap<String, String>,
        backend_available: bool,
    ) -> Self {
        let enabled = read_bool(opts, tmux::SIDEBAR_NOTIFICATIONS).unwrap_or(true);
        let enabled = enabled && backend_available;
        let events = opts
            .get(tmux::SIDEBAR_NOTIFICATIONS_EVENTS)
            .map_or_else(|| Self::default().events, |raw| parse_events(raw));
        let sound = parse_sound(opts.get(tmux::SIDEBAR_NOTIFICATION_SOUND));

        Self {
            enabled,
            events,
            sound,
        }
    }

    pub fn from_tmux() -> Self {
        Self::from_tmux_options(&tmux::get_all_global_options())
    }

    pub fn event_enabled(&self, event: DesktopNotificationEvent) -> bool {
        self.events.contains(&event)
    }
}

fn parse_events(raw: &str) -> HashSet<DesktopNotificationEvent> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return HashSet::new();
    }
    if trimmed.eq_ignore_ascii_case("all") {
        return DesktopNotificationEvent::ALL.iter().copied().collect();
    }
    trimmed
        .split(',')
        .filter_map(DesktopNotificationEvent::from_token)
        .collect()
}

/// Parse the `@sidebar_notification_sound` option into an optional sound name.
///
/// - unset / empty / `off` / `false` / `none` / `no` → `None` (silent, default)
/// - `on` / `true` / `yes` / `default` → the platform [`DEFAULT_SOUND_NAME`]
/// - anything else → that literal value, used verbatim as the sound name
fn parse_sound(raw: Option<&String>) -> Option<String> {
    let trimmed = raw.map(|s| s.trim()).unwrap_or("");
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "off" | "false" | "none" | "no" | "0" => None,
        "on" | "true" | "yes" | "default" | "1" => Some(DEFAULT_SOUND_NAME.to_string()),
        _ => Some(trimmed.to_string()),
    }
}

pub fn format_title(repo: Option<&str>, branch: Option<&str>, agent: &str) -> String {
    let repo = repo.map(str::trim).filter(|s| !s.is_empty());
    let branch = branch.map(str::trim).filter(|s| !s.is_empty());
    match (repo, branch) {
        (Some(repo), Some(branch)) => format!("{repo} ({branch}) / {agent}"),
        (Some(repo), None) => format!("{repo} / {agent}"),
        _ => agent.to_string(),
    }
}

pub fn run_scoped_fingerprint(started_at: Option<u64>, fingerprint: &str) -> String {
    match started_at {
        Some(started_at) => format!("{started_at}:{fingerprint}"),
        None => fingerprint.to_string(),
    }
}

/// Returns true if a notification of `kind` has already fired for the
/// current `run_id` on this pane. Use to dedupe events that share a kind
/// but use distinct fingerprints (e.g. `Stop` vs explicit `TaskCompleted`
/// in the same run).
pub fn has_run_scoped_stamp(
    pane_id: &str,
    kind: DesktopNotificationKind,
    run_id: Option<u64>,
) -> bool {
    let Some(run_id) = run_id else { return false };
    if pane_id.is_empty() {
        return false;
    }
    let raw = tmux::get_pane_option_value(pane_id, stamp_option_key(kind));
    let Some(stamp) = parse_stamp(&raw) else {
        return false;
    };
    stamp.fingerprint.starts_with(&format!("{run_id}:"))
}

pub fn notify_if_allowed(
    settings: &DesktopNotificationSettings,
    pane_id: &str,
    kind: DesktopNotificationKind,
    event: DesktopNotificationEvent,
    fingerprint: &str,
    title: &str,
    body: &str,
) -> bool {
    if !settings.enabled || pane_id.is_empty() || !settings.event_enabled(event) {
        return false;
    }

    let key = stamp_option_key(kind);
    let normalized_fingerprint = normalize_fingerprint(fingerprint);
    let now = now_epoch_secs();
    let current = tmux::get_pane_option_value(pane_id, key);
    if let Some(stamp) = parse_stamp(&current)
        && stamp.fingerprint == normalized_fingerprint
        && now.saturating_sub(stamp.timestamp) < DESKTOP_NOTIFICATION_COOLDOWN_SECS
    {
        return false;
    }

    match send_desktop_notification(title, body, settings.sound.as_deref()) {
        Ok(()) => {
            tmux::set_pane_option(pane_id, key, &encode_stamp(now, &normalized_fingerprint));
            true
        }
        Err(err) => {
            eprintln!("desktop notification failed: {err}");
            false
        }
    }
}

fn read_bool(opts: &HashMap<String, String>, key: &str) -> Option<bool> {
    let raw = opts.get(key)?.trim().to_ascii_lowercase();
    match raw.as_str() {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

struct NotificationStamp {
    timestamp: u64,
    fingerprint: String,
}

fn stamp_option_key(kind: DesktopNotificationKind) -> &'static str {
    match kind {
        DesktopNotificationKind::TaskCompleted => tmux::PANE_OS_NOTIFY_TASK_COMPLETED,
        DesktopNotificationKind::TaskFailed => tmux::PANE_OS_NOTIFY_TASK_FAILED,
        DesktopNotificationKind::PermissionRequired => tmux::PANE_OS_NOTIFY_PERMISSION_REQUIRED,
    }
}

fn encode_stamp(timestamp: u64, fingerprint: &str) -> String {
    format!("{}|{}", timestamp, fingerprint)
}

fn parse_stamp(raw: &str) -> Option<NotificationStamp> {
    let (ts, fingerprint) = raw.split_once('|')?;
    Some(NotificationStamp {
        timestamp: ts.parse().ok()?,
        fingerprint: fingerprint.to_string(),
    })
}

fn normalize_fingerprint(value: &str) -> String {
    value.replace(['|', '\n', '\r'], " ")
}

fn send_desktop_notification(title: &str, body: &str, sound: Option<&str>) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let script = build_applescript(title, body);
        let mut command = Command::new("osascript");
        command
            .args(["-e", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let result =
            run_notification_command(&mut command, "osascript", DESKTOP_NOTIFICATION_TIMEOUT);
        // Play the sound with `afplay` rather than the AppleScript
        // `sound name` clause. `display notification ... sound name` routes
        // through the macOS *alert* volume (often 0) and is silenced by Focus
        // / per-app notification-sound settings, so the banner shows but is
        // mute. `afplay` uses the main output volume and always plays.
        // Fire-and-forget: a failed play must never fail the notification.
        if result.is_ok()
            && let Some(path) = sound.and_then(resolve_macos_sound_path)
        {
            let _ = Command::new("afplay")
                .arg(&path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        result
    }

    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("notify-send");
        command
            .args([
                "--app-name=tmux-agent-sidebar",
                "--urgency=normal",
                title,
                body,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let result =
            run_notification_command(&mut command, "notify-send", DESKTOP_NOTIFICATION_TIMEOUT);
        // notify-send has no portable sound argument, so play a best-effort
        // sound via libcanberra when one is configured. Fire-and-forget: a
        // missing player must never fail the visual notification we just sent.
        if result.is_ok()
            && let Some(name) = sound.map(str::trim).filter(|s| !s.is_empty())
        {
            let _ = Command::new("canberra-gtk-play")
                .args(["-i", name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        result
    }

    #[cfg(target_os = "windows")]
    {
        let _ = (title, body, sound);
        Err("desktop notifications are not supported on Windows yet".into())
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (title, body, sound);
        Err("desktop notifications are not supported on this platform".into())
    }
}

/// Build the AppleScript for a macOS `display notification` (visual only —
/// sound is played separately via `afplay`). Factored out so the escaping is
/// unit-testable without spawning `osascript`.
#[cfg(target_os = "macos")]
fn build_applescript(title: &str, body: &str) -> String {
    format!(
        "display notification \"{}\" with title \"{}\"",
        escape_applescript(body),
        escape_applescript(title)
    )
}

/// Resolve a configured sound value to a file path `afplay` can play.
///
/// - a value containing `/` is treated as a path and returned only if it exists
/// - a bare name (e.g. `Glass`) is resolved against the system and user sound
///   libraries (`/System/Library/Sounds`, `~/Library/Sounds`), `.aiff` first
///
/// Returns `None` when nothing matches, so an unknown name is silently a no-op.
#[cfg(target_os = "macos")]
fn resolve_macos_sound_path(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if name.contains('/') {
        return std::path::Path::new(name)
            .is_file()
            .then(|| name.to_string());
    }
    let home = std::env::var("HOME").unwrap_or_default();
    [
        format!("/System/Library/Sounds/{name}.aiff"),
        format!("{home}/Library/Sounds/{name}.aiff"),
    ]
    .into_iter()
    .find(|candidate| std::path::Path::new(candidate).is_file())
}

fn notification_backend_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("osascript");
        command
            .args(["-e", "return 0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        run_notification_command(
            &mut command,
            "osascript",
            DESKTOP_NOTIFICATION_PROBE_TIMEOUT,
        )
        .is_ok()
    }

    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("notify-send");
        command
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        return run_notification_command(
            &mut command,
            "notify-send",
            DESKTOP_NOTIFICATION_PROBE_TIMEOUT,
        )
        .is_ok();
    }

    #[cfg(target_os = "windows")]
    {
        false
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
fn escape_applescript(value: &str) -> String {
    value
        .replace(['\n', '\r'], " ")
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn run_notification_command(
    command: &mut Command,
    command_name: &str,
    timeout: Duration,
) -> Result<(), String> {
    let mut child = command
        .spawn()
        .map_err(|err| format!("failed to spawn {command_name}: {err}"))?;
    let start = Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(format!("{command_name} exited with status {status}"));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{command_name} timed out after {}s",
                        timeout.as_secs()
                    ));
                }
                sleep(Duration::from_millis(25));
            }
            Err(err) => return Err(format!("failed to wait on {command_name}: {err}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_bool_and_numbers() {
        let mut opts = HashMap::new();
        opts.insert(tmux::SIDEBAR_NOTIFICATIONS.into(), "on".into());

        let settings = DesktopNotificationSettings::from_tmux_options_with_backend(&opts, true);
        assert!(settings.enabled);
    }

    #[test]
    fn settings_default_when_invalid() {
        let mut opts = HashMap::new();
        opts.insert(tmux::SIDEBAR_NOTIFICATIONS.into(), "maybe".into());

        let settings = DesktopNotificationSettings::from_tmux_options_with_backend(&opts, true);
        assert!(settings.enabled);
    }

    #[test]
    fn settings_disable_when_off() {
        let mut opts = HashMap::new();
        opts.insert(tmux::SIDEBAR_NOTIFICATIONS.into(), "off".into());

        let settings = DesktopNotificationSettings::from_tmux_options_with_backend(&opts, true);
        assert!(!settings.enabled);
    }

    #[test]
    fn settings_disable_when_backend_missing() {
        let opts = HashMap::new();
        let settings = DesktopNotificationSettings::from_tmux_options_with_backend(&opts, false);
        assert!(!settings.enabled);
    }

    #[test]
    fn parse_sound_off_variants_return_none() {
        for raw in ["", "   ", "off", "Off", "false", "none", "no", "0"] {
            assert_eq!(parse_sound(Some(&raw.to_string())), None, "{raw:?}");
        }
        assert_eq!(parse_sound(None), None);
    }

    #[test]
    fn parse_sound_on_variants_use_default_name() {
        for raw in ["on", "ON", "true", "yes", "default", "1"] {
            assert_eq!(
                parse_sound(Some(&raw.to_string())).as_deref(),
                Some(DEFAULT_SOUND_NAME),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn parse_sound_custom_name_is_used_verbatim() {
        assert_eq!(
            parse_sound(Some(&"Glass".to_string())).as_deref(),
            Some("Glass")
        );
        assert_eq!(
            parse_sound(Some(&"  Submarine  ".to_string())).as_deref(),
            Some("Submarine")
        );
    }

    #[test]
    fn settings_parse_sound_from_option() {
        let mut opts = HashMap::new();
        opts.insert(tmux::SIDEBAR_NOTIFICATION_SOUND.into(), "Glass".into());
        let settings = DesktopNotificationSettings::from_tmux_options_with_backend(&opts, true);
        assert_eq!(settings.sound.as_deref(), Some("Glass"));
    }

    #[test]
    fn settings_sound_defaults_to_none_when_unset() {
        let opts = HashMap::new();
        let settings = DesktopNotificationSettings::from_tmux_options_with_backend(&opts, true);
        assert_eq!(settings.sound, None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn build_applescript_formats_visual_notification() {
        assert_eq!(
            build_applescript("Title", "Body"),
            "display notification \"Body\" with title \"Title\""
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn resolve_macos_sound_path_resolves_system_sound_name() {
        // Glass ships with every macOS install.
        assert_eq!(
            resolve_macos_sound_path("Glass").as_deref(),
            Some("/System/Library/Sounds/Glass.aiff")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn resolve_macos_sound_path_unknown_or_blank_is_none() {
        assert_eq!(resolve_macos_sound_path("NoSuchSound12345"), None);
        assert_eq!(resolve_macos_sound_path("   "), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn resolve_macos_sound_path_absolute_path_passthrough_when_exists() {
        assert_eq!(
            resolve_macos_sound_path("/System/Library/Sounds/Glass.aiff").as_deref(),
            Some("/System/Library/Sounds/Glass.aiff")
        );
        assert_eq!(resolve_macos_sound_path("/no/such/file.aiff"), None);
    }

    #[test]
    fn format_title_variants() {
        assert_eq!(
            format_title(Some("repo"), Some("feat/xyz"), "claude"),
            "repo (feat/xyz) / claude"
        );
        assert_eq!(format_title(Some("repo"), None, "claude"), "repo / claude");
        assert_eq!(
            format_title(Some("repo"), Some(""), "claude"),
            "repo / claude"
        );
        assert_eq!(format_title(None, Some("feat"), "claude"), "claude");
        assert_eq!(format_title(None, None, "claude"), "claude");
    }

    #[test]
    fn stamp_round_trip() {
        let stamp = encode_stamp(123, "foo bar");
        let parsed = parse_stamp(&stamp).unwrap();
        assert_eq!(parsed.timestamp, 123);
        assert_eq!(parsed.fingerprint, "foo bar");
    }

    #[test]
    fn fingerprint_is_normalized() {
        assert_eq!(
            normalize_fingerprint("foo|bar\nbaz\rqux"),
            "foo bar baz qux"
        );
    }

    #[test]
    fn events_default_to_default_set_when_unset() {
        let opts = HashMap::new();
        let settings = DesktopNotificationSettings::from_tmux_options_with_backend(&opts, true);
        for event in DesktopNotificationEvent::DEFAULT {
            assert!(settings.event_enabled(event), "expected {event:?} enabled");
        }
        assert!(
            !settings.event_enabled(DesktopNotificationEvent::TaskCompleted),
            "task_completed should be opt-in"
        );
    }

    #[test]
    fn events_parse_explicit_subset() {
        let mut opts = HashMap::new();
        opts.insert(
            tmux::SIDEBAR_NOTIFICATIONS_EVENTS.into(),
            "stop, permission_denied".into(),
        );
        let settings = DesktopNotificationSettings::from_tmux_options_with_backend(&opts, true);
        assert!(settings.event_enabled(DesktopNotificationEvent::Stop));
        assert!(settings.event_enabled(DesktopNotificationEvent::PermissionDenied));
        assert!(!settings.event_enabled(DesktopNotificationEvent::Notification));
        assert!(!settings.event_enabled(DesktopNotificationEvent::TaskCompleted));
        assert!(!settings.event_enabled(DesktopNotificationEvent::StopFailure));
    }

    #[test]
    fn events_all_keyword_enables_every_event() {
        let mut opts = HashMap::new();
        opts.insert(tmux::SIDEBAR_NOTIFICATIONS_EVENTS.into(), "all".into());
        let settings = DesktopNotificationSettings::from_tmux_options_with_backend(&opts, true);
        for event in DesktopNotificationEvent::ALL {
            assert!(settings.event_enabled(event));
        }
    }

    #[test]
    fn events_empty_value_disables_every_event() {
        let mut opts = HashMap::new();
        opts.insert(tmux::SIDEBAR_NOTIFICATIONS_EVENTS.into(), "".into());
        let settings = DesktopNotificationSettings::from_tmux_options_with_backend(&opts, true);
        for event in DesktopNotificationEvent::ALL {
            assert!(!settings.event_enabled(event));
        }
    }

    #[test]
    fn events_unknown_tokens_are_ignored() {
        let mut opts = HashMap::new();
        opts.insert(
            tmux::SIDEBAR_NOTIFICATIONS_EVENTS.into(),
            "stop,bogus, task_completed".into(),
        );
        let settings = DesktopNotificationSettings::from_tmux_options_with_backend(&opts, true);
        assert!(settings.event_enabled(DesktopNotificationEvent::Stop));
        assert!(settings.event_enabled(DesktopNotificationEvent::TaskCompleted));
        assert!(!settings.event_enabled(DesktopNotificationEvent::Notification));
    }

    #[test]
    fn has_run_scoped_stamp_returns_false_without_stamp() {
        let _guard = tmux::test_mock::install();
        let pane = "%PANE_NO_STAMP";
        assert!(!has_run_scoped_stamp(
            pane,
            DesktopNotificationKind::TaskCompleted,
            Some(1_700_000_000_000),
        ));
    }

    #[test]
    fn has_run_scoped_stamp_matches_current_run() {
        let _guard = tmux::test_mock::install();
        let pane = "%PANE_CURRENT_RUN";
        let run_id = 1_700_000_000_000_u64;
        let stamp = encode_stamp(42, &format!("{run_id}:task-xyz"));
        tmux::test_mock::set(
            pane,
            stamp_option_key(DesktopNotificationKind::TaskCompleted),
            &stamp,
        );
        assert!(has_run_scoped_stamp(
            pane,
            DesktopNotificationKind::TaskCompleted,
            Some(run_id),
        ));
    }

    #[test]
    fn has_run_scoped_stamp_rejects_stale_run() {
        let _guard = tmux::test_mock::install();
        let pane = "%PANE_STALE_RUN";
        let old_run = 1_600_000_000_000_u64;
        let new_run = 1_700_000_000_000_u64;
        let stamp = encode_stamp(42, &format!("{old_run}:task-xyz"));
        tmux::test_mock::set(
            pane,
            stamp_option_key(DesktopNotificationKind::TaskCompleted),
            &stamp,
        );
        assert!(!has_run_scoped_stamp(
            pane,
            DesktopNotificationKind::TaskCompleted,
            Some(new_run),
        ));
    }

    #[test]
    fn has_run_scoped_stamp_requires_run_id() {
        let _guard = tmux::test_mock::install();
        let pane = "%PANE_NO_RUN_ID";
        let stamp = encode_stamp(42, "1700000000000:task-xyz");
        tmux::test_mock::set(
            pane,
            stamp_option_key(DesktopNotificationKind::TaskCompleted),
            &stamp,
        );
        assert!(!has_run_scoped_stamp(
            pane,
            DesktopNotificationKind::TaskCompleted,
            None,
        ));
    }
}
