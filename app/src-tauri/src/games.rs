//! Game detection: watches the foreground window and decides whether it belongs to a game.
//!
//! A detected game stays "current" until its process exits, so alt-tabbing to Discord and
//! pressing the save hotkey still files the clip under the game.

use std::path::Path;
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, RECT, STILL_ACTIVE};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetForegroundWindow, GetWindowLongW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsZoomed,
    WS_CAPTION,
};
use windows::core::PWSTR;

/// Well-known game executables (lowercase) and the name clips are filed under.
const KNOWN: &[(&str, &str)] = &[
    ("valorant-win64-shipping.exe", "VALORANT"),
    ("cs2.exe", "Counter-Strike 2"),
    ("fortniteclient-win64-shipping.exe", "Fortnite"),
    ("r5apex.exe", "Apex Legends"),
    ("r5apex_dx12.exe", "Apex Legends"),
    ("league of legends.exe", "League of Legends"),
    ("overwatch.exe", "Overwatch 2"),
    ("rocketleague.exe", "Rocket League"),
    ("minecraft.windows.exe", "Minecraft"),
    ("gta5.exe", "Grand Theft Auto V"),
    ("gta5_enhanced.exe", "Grand Theft Auto V"),
    ("rainbowsix.exe", "Rainbow Six Siege"),
    ("rainbowsix_vulkan.exe", "Rainbow Six Siege"),
    ("dota2.exe", "Dota 2"),
    ("destiny2.exe", "Destiny 2"),
    ("eldenring.exe", "Elden Ring"),
    ("cod.exe", "Call of Duty"),
    ("robloxplayerbeta.exe", "Roblox"),
    ("marvel-win64-shipping.exe", "Marvel Rivals"),
    ("discovery.exe", "THE FINALS"),
    ("helldivers2.exe", "Helldivers 2"),
    ("escapefromtarkov.exe", "Escape from Tarkov"),
    ("tslgame.exe", "PUBG"),
    ("osu!.exe", "osu!"),
    ("wow.exe", "World of Warcraft"),
    ("deadlock.exe", "Deadlock"),
    ("project8.exe", "Deadlock"),
    ("rustclient.exe", "Rust"),
    ("thefinals.exe", "THE FINALS"),
];

/// Never games, even when fullscreen or inside a store folder.
const NOT_GAMES: &[&str] = &[
    "explorer.exe",
    "steam.exe",
    "steamwebhelper.exe",
    "epicgameslauncher.exe",
    "riotclientservices.exe",
    "riotclientux.exe",
    "riot client.exe",
    "leagueclient.exe",
    "leagueclientux.exe",
    "battle.net.exe",
    "eadesktop.exe",
    "upc.exe",
    "galaxyclient.exe",
    "discord.exe",
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "opera.exe",
    "opera_gx.exe",
    "brave.exe",
    "vivaldi.exe",
    "zen.exe",
    "arc.exe",
    "floorp.exe",
    "librewolf.exe",
    "waterfox.exe",
    "thorium.exe",
    "iexplore.exe",
    "msedgewebview2.exe",
    "netflix.exe",
    "potplayermini64.exe",
    "teams.exe",
    "ms-teams.exe",
    "slack.exe",
    "steelseriesgg.exe",
    "nvidia app.exe",
    "nvcontainer.exe",
    "medal.exe",
    "vlc.exe",
    "mpc-hc64.exe",
    "mpv.exe",
    "obs64.exe",
    "spotify.exe",
    "code.exe",
    "wallpaper32.exe",
    "wallpaper64.exe",
    "bettr-app.exe",
    "bettr-engine.exe",
    "applicationframehost.exe",
    "searchhost.exe",
    "startmenuexperiencehost.exe",
    "shellexperiencehost.exe",
    "lockapp.exe",
    "textinputhost.exe",
];

/// Store install folders: the game name is the folder right after the marker.
const STORE_DIRS: &[&str] = &[
    "\\steamapps\\common\\",
    "\\epic games\\",
    "\\riot games\\",
    "\\xboxgames\\",
    "\\ubisoft game launcher\\games\\",
    "\\ea games\\",
    "\\gog galaxy\\games\\",
    "\\battle.net\\",
];

const NOT_GAME_DIRS: &[&str] = &["riot client", "launcher", "steamworks shared", "wallpaper_engine", "directx"];

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Game {
    pub name: String,
    pub exe: String,
    #[serde(skip)]
    pub pid: u32,
}

/// Name for a game process, or `None` if it doesn't look like one.
pub fn classify(path: &str, fullscreen: bool, title: &str) -> Option<String> {
    let exe = Path::new(path).file_name()?.to_string_lossy().to_lowercase();
    if NOT_GAMES.contains(&exe.as_str()) {
        return None;
    }
    if exe == "javaw.exe" {
        return title.starts_with("Minecraft").then(|| "Minecraft".to_owned());
    }
    if let Some((_, name)) = KNOWN.iter().find(|(e, _)| *e == exe) {
        return Some((*name).to_owned());
    }
    let lower = path.to_lowercase();
    for marker in STORE_DIRS {
        if let Some(i) = lower.find(marker) {
            let start = i + marker.len();
            let folder = path[start..].split('\\').next().unwrap_or_default();
            if !folder.is_empty() && !NOT_GAME_DIRS.iter().any(|n| folder.to_lowercase().contains(n)) {
                return Some(folder.to_owned());
            }
        }
    }
    // Unknown program covering the whole monitor: probably a game. Name it after its window.
    if fullscreen && !lower.contains("\\windows\\") {
        let t = title.trim();
        return Some(if t.is_empty() {
            Path::new(path).file_stem()?.to_string_lossy().into_owned()
        } else {
            t.to_owned()
        });
    }
    None
}

fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

fn process_alive(pid: u32) -> bool {
    unsafe {
        let Ok(h): Result<HANDLE, _> = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code).is_ok();
        let _ = CloseHandle(h);
        ok && code == STILL_ACTIVE.0 as u32
    }
}

/// Borderless or exclusive fullscreen: no title bar, not maximized, covering its monitor.
/// (A maximized window also overhangs the monitor by its borders, so that alone means nothing.)
fn covers_monitor(hwnd: HWND) -> bool {
    unsafe {
        if IsZoomed(hwnd).as_bool() || (GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_CAPTION.0) == WS_CAPTION.0 {
            return false;
        }
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_err() {
            return false;
        }
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return false;
        }
        let m = mi.rcMonitor;
        r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
    }
}

fn foreground_game() -> Option<Game> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 || pid == std::process::id() {
            return None;
        }
        let path = process_path(pid)?;
        let mut title = [0u16; 256];
        let n = GetWindowTextW(hwnd, &mut title);
        let title = String::from_utf16_lossy(&title[..n.max(0) as usize]);
        let name = classify(&path, covers_monitor(hwnd), &title)?;
        let exe = Path::new(&path).file_name()?.to_string_lossy().into_owned();
        Some(Game { name, exe, pid })
    }
}

/// Polls every 1.5 s and calls `on_change` when the current game changes.
/// `enabled` is re-read every tick so the setting applies immediately.
pub fn spawn_detector(
    enabled: impl Fn() -> bool + Send + 'static,
    on_change: impl Fn(Option<Game>) + Send + 'static,
) {
    std::thread::Builder::new()
        .name("game-detect".into())
        .spawn(move || {
            let mut current: Option<Game> = None;
            loop {
                let next = if !enabled() {
                    None
                } else {
                    match foreground_game() {
                        Some(g) => Some(g),
                        None => current.clone().filter(|g| process_alive(g.pid)),
                    }
                };
                if next != current {
                    tracing::info!(game = ?next.as_ref().map(|g| &g.name), "game changed");
                    current = next;
                    on_change(current.clone());
                }
                std::thread::sleep(Duration::from_millis(1500));
            }
        })
        .expect("spawn game detector");
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn known_and_store_games() {
        let v = r"C:\Riot Games\VALORANT\live\ShooterGame\Binaries\Win64\VALORANT-Win64-Shipping.exe";
        assert_eq!(classify(v, false, "VALORANT  ").as_deref(), Some("VALORANT"));
        let s = r"D:\SteamLibrary\steamapps\common\Hades II\Ship\Hades2.exe";
        assert_eq!(classify(s, false, "").as_deref(), Some("Hades II"));
        let e = r"C:\Program Files\Epic Games\Satisfactory\FactoryGame.exe";
        assert_eq!(classify(e, false, "").as_deref(), Some("Satisfactory"));
    }

    #[test]
    fn launchers_browsers_and_windows_are_not_games() {
        assert_eq!(classify(r"C:\Riot Games\Riot Client\RiotClientServices.exe", true, ""), None);
        assert_eq!(classify(r"C:\Riot Games\Riot Client\UX\Something.exe", false, ""), None);
        assert_eq!(classify(r"C:\Program Files\Google\Chrome\Application\chrome.exe", true, "YouTube"), None);
        assert_eq!(classify(r"C:\Windows\System32\mmc.exe", true, "Console"), None);
        assert_eq!(classify(r"C:\Program Files\Zen Browser\zen.exe", true, "YouTube"), None);
        assert_eq!(classify(r"C:\Tools\notepad++.exe", false, "notes"), None);
    }

    #[test]
    fn fullscreen_unknown_uses_title() {
        assert_eq!(classify(r"C:\Games\Indie\game.exe", true, "Tiny Game").as_deref(), Some("Tiny Game"));
        assert_eq!(classify(r"C:\Games\Indie\game.exe", true, "").as_deref(), Some("game"));
        assert_eq!(
            classify(r"C:\Program Files\Java\bin\javaw.exe", false, "Minecraft 1.21").as_deref(),
            Some("Minecraft")
        );
    }
}
