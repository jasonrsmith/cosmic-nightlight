//! cosmic-nightlight: night light for COSMIC using hardware gamma ramps.
//!
//! Talks `wlr-gamma-control-unstable-v1` to the compositor, which programs the
//! display's GAMMA_LUT. Colour maths:
//!   * the white point for a colour temperature comes from the CIE Planckian
//!     locus (Kim et al. 2002 cubic-spline approximation), converted
//!     xyY -> XYZ -> linear sRGB and normalised to the daytime white;
//!   * the per-channel scaling is applied in *linear light* (decode sRGB,
//!     scale, re-encode), so shadows keep their hue and blacks stay black;
//!   * changes fade smoothly in mired space (perceptually even steps).

use std::{
    fs::{self, File},
    io::Write,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use calloop::{
    EventLoop, LoopHandle,
    signals::{Signal, Signals},
    timer::{TimeoutAction, Timer},
};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    delegate_registry,
    dispatch2::Dispatch2,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
};
use wayland_client::{
    Connection, Proxy, QueueHandle,
    globals::{GlobalList, registry_queue_init},
    protocol::wl_output,
};
use wayland_protocols_wlr::gamma_control::v1::client::{
    zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1,
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Config {
    lat: f64,
    lon: f64,
    temp_day: f64,
    temp_night: f64,
    brightness_day: f64,
    brightness_night: f64,
    elevation_day: f64,
    elevation_night: f64,
    fade_seconds: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            lat: 51.48,
            lon: 0.0,
            temp_day: 6500.0,
            temp_night: 3700.0,
            brightness_day: 1.0,
            brightness_night: 1.0,
            elevation_day: 3.0,
            elevation_night: -6.0,
            fade_seconds: 1.5,
        }
    }
}

const DEFAULT_CONFIG: &str = "\
# cosmic-nightlight config. Apply changes with: cosmic-nightlight reload
# Your location (decimal degrees, west/south negative). Used for sunset/sunrise.
# CHANGE THESE: the defaults are Greenwich, London.
lat = 51.48
lon = 0.0
# Colour temperature in Kelvin. 6500 = neutral (no change).
temp_day = 6500
temp_night = 3700
# Optional dimming, 0.1-1.0 (applied in linear light, blacks stay black).
brightness_day = 1.0
brightness_night = 1.0
# Transition window by sun elevation in degrees (redshift defaults).
elevation_day = 3
elevation_night = -6
# Length of fades when toggling / starting, in seconds.
fade_seconds = 1.5
";

fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
    base.join("cosmic-nightlight").join("config")
}

fn load_config() -> Config {
    let path = config_path();
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            if let Some(dir) = path.parent() {
                let _ = fs::create_dir_all(dir);
            }
            let _ = fs::write(&path, DEFAULT_CONFIG);
            DEFAULT_CONFIG.to_string()
        }
    };
    let mut c = Config::default();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            eprintln!("config line {}: expected key = value", n + 1);
            continue;
        };
        let Ok(v) = v.trim().parse::<f64>() else {
            eprintln!("config line {}: '{}' is not a number", n + 1, v.trim());
            continue;
        };
        match k.trim() {
            "lat" => c.lat = v,
            "lon" => c.lon = v,
            "temp_day" => c.temp_day = v.clamp(1700.0, 25000.0),
            "temp_night" => c.temp_night = v.clamp(1700.0, 25000.0),
            "brightness_day" => c.brightness_day = v.clamp(0.1, 1.0),
            "brightness_night" => c.brightness_night = v.clamp(0.1, 1.0),
            "elevation_day" => c.elevation_day = v,
            "elevation_night" => c.elevation_night = v,
            "fade_seconds" => c.fade_seconds = v.clamp(0.0, 30.0),
            "strength" => {} // obsolete (overlay version)
            other => eprintln!("config line {}: unknown key '{other}'", n + 1),
        }
    }
    c
}

// ---------------------------------------------------------------------------
// Sun position
// ---------------------------------------------------------------------------

/// Solar elevation in degrees (low-precision Astronomical Almanac formulae).
fn solar_elevation(unix: f64, lat: f64, lon: f64) -> f64 {
    let n = (unix - 946_728_000.0) / 86_400.0; // days since J2000.0
    let l = (280.460 + 0.985_647_4 * n).rem_euclid(360.0);
    let g = (357.528 + 0.985_600_3 * n).rem_euclid(360.0).to_radians();
    let lambda = (l + 1.915 * g.sin() + 0.020 * (2.0 * g).sin()).to_radians();
    let eps = (23.439 - 0.000_000_4 * n).to_radians();
    let ra = (eps.cos() * lambda.sin()).atan2(lambda.cos());
    let dec = (eps.sin() * lambda.sin()).asin();
    let gmst_deg = (18.697_374_558 + 24.065_709_824_419_08 * n) * 15.0;
    let ha = (gmst_deg + lon).to_radians() - ra;
    let lat = lat.to_radians();
    (lat.sin() * dec.sin() + lat.cos() * dec.cos() * ha.cos()).asin().to_degrees()
}

fn now_unix() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

/// How far into "day" we are: 0.0 = full night, 1.0 = full day.
fn day_fraction(cfg: &Config, unix: f64) -> (f64, f64) {
    let elev = solar_elevation(unix, cfg.lat, cfg.lon);
    let t = ((elev - cfg.elevation_night) / (cfg.elevation_day - cfg.elevation_night)).clamp(0.0, 1.0);
    (t, elev)
}

// ---------------------------------------------------------------------------
// Colour science
// ---------------------------------------------------------------------------

/// A display setting: colour temperature (K) and linear-light brightness.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Setting {
    temp: f64,
    brightness: f64,
}

impl Setting {
    fn lerp_mired(a: Setting, b: Setting, t: f64) -> Setting {
        let (ma, mb) = (1e6 / a.temp, 1e6 / b.temp);
        Setting {
            temp: 1e6 / (ma + (mb - ma) * t),
            brightness: a.brightness + (b.brightness - a.brightness) * t,
        }
    }
    fn close_to(&self, other: &Setting) -> bool {
        (1e6 / self.temp - 1e6 / other.temp).abs() < 0.5 && (self.brightness - other.brightness).abs() < 0.002
    }
}

fn scheduled_setting(cfg: &Config, unix: f64) -> Setting {
    let (t, _) = day_fraction(cfg, unix);
    Setting::lerp_mired(
        Setting { temp: cfg.temp_night, brightness: cfg.brightness_night },
        Setting { temp: cfg.temp_day, brightness: cfg.brightness_day },
        t,
    )
}

/// CIE 1931 xy chromaticity of a blackbody (Kim et al. 2002), 1667 K..25000 K.
fn planckian_xy(temp: f64) -> (f64, f64) {
    let t = temp.clamp(1667.0, 25000.0);
    let (t2, t3) = (t * t, t * t * t);
    let x = if t <= 4000.0 {
        -0.266_123_9e9 / t3 - 0.234_358_9e6 / t2 + 0.877_695_6e3 / t + 0.179_910
    } else {
        -3.025_846_9e9 / t3 + 2.107_037_9e6 / t2 + 0.222_634_7e3 / t + 0.240_390
    };
    let (x2, x3) = (x * x, x * x * x);
    let y = if t <= 2222.0 {
        -1.106_381_4 * x3 - 1.348_110_20 * x2 + 2.185_558_32 * x - 0.202_196_83
    } else if t <= 4000.0 {
        -0.954_947_6 * x3 - 1.374_185_93 * x2 + 2.091_370_15 * x - 0.167_488_67
    } else {
        3.081_758_0 * x3 - 5.873_386_70 * x2 + 3.751_129_97 * x - 0.370_014_83
    };
    (x, y)
}

/// Linear sRGB (unnormalised) of a blackbody's white point.
fn blackbody_linear_rgb(temp: f64) -> [f64; 3] {
    let (x, y) = planckian_xy(temp);
    let (cx, cy, cz) = (x / y, 1.0, (1.0 - x - y) / y);
    [
        3.240_454_2 * cx - 1.537_138_5 * cy - 0.498_531_4 * cz,
        -0.969_266_0 * cx + 1.876_010_8 * cy + 0.041_556_0 * cz,
        0.055_643_4 * cx - 0.204_025_9 * cy + 1.057_225_2 * cz,
    ]
    .map(|v| v.max(0.0))
}

/// Per-channel linear-light multipliers for `temp`, relative to `reference`
/// (the daytime white, which maps to identity). Max channel is 1.0.
fn white_point_gains(temp: f64, reference: f64) -> [f64; 3] {
    let t = blackbody_linear_rgb(temp);
    let r = blackbody_linear_rgb(reference);
    let g = [0, 1, 2].map(|i| t[i] / r[i]);
    let max = g.iter().cloned().fold(f64::MIN, f64::max);
    g.map(|v| v / max)
}

fn srgb_to_linear(v: f64) -> f64 {
    if v <= 0.040_45 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

fn linear_to_srgb(v: f64) -> f64 {
    if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

/// Gamma ramp in wlr-gamma-control layout: `size` red, green, blue u16s.
fn build_ramp(setting: Setting, reference: f64, size: usize) -> Vec<u16> {
    let gains = white_point_gains(setting.temp, reference).map(|g| g * setting.brightness);
    let mut ramp = Vec::with_capacity(size * 3);
    for gain in gains {
        for i in 0..size {
            let v = i as f64 / (size - 1) as f64;
            let out = linear_to_srgb(srgb_to_linear(v) * gain).clamp(0.0, 1.0);
            ramp.push((out * 65535.0).round() as u16);
        }
    }
    ramp
}

// ---------------------------------------------------------------------------
// Wayland
// ---------------------------------------------------------------------------

struct ManagerData;
struct ControlData;

struct Head {
    output: wl_output::WlOutput,
    control: ZwlrGammaControlV1,
    size: Option<usize>,
    failed: bool,
    applied: Option<Setting>,
}

#[derive(Clone, Copy)]
struct Fade {
    from: Setting,
    to: Setting,
    start: Instant,
    duration: Duration,
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    manager: ZwlrGammaControlManagerV1,
    conn: Connection,
    qh: QueueHandle<App>,
    loop_handle: LoopHandle<'static, App>,
    heads: Vec<Head>,

    cfg: Config,
    fixed_temp: Option<f64>,
    paused: bool,
    current: Setting,
    fade: Option<Fade>,
    fade_timer_running: bool,
    warned_failed: bool,
    exit_after_fade: bool,
    exit: bool,
}

impl App {
    fn neutral(&self) -> Setting {
        Setting { temp: self.cfg.temp_day, brightness: 1.0 }
    }

    fn target(&self) -> Setting {
        if self.paused || self.exit_after_fade {
            return self.neutral();
        }
        match self.fixed_temp {
            Some(temp) => Setting { temp, brightness: 1.0 },
            None => scheduled_setting(&self.cfg, now_unix()),
        }
    }

    fn add_output(&mut self, output: wl_output::WlOutput) {
        if self.heads.iter().any(|h| h.output == output) {
            return;
        }
        let control = self.manager.get_gamma_control(&output, &self.qh, ControlData);
        self.heads.push(Head { output, control, size: None, failed: false, applied: None });
    }

    fn apply(&mut self, setting: Setting) {
        self.current = setting;
        let reference = self.cfg.temp_day;
        for head in &mut self.heads {
            let (Some(size), false) = (head.size, head.failed) else { continue };
            if head.applied.is_some_and(|a| a == setting) {
                continue;
            }
            match ramp_file(&build_ramp(setting, reference, size)) {
                Ok(file) => {
                    head.control.set_gamma(std::os::fd::AsFd::as_fd(&file));
                    head.applied = Some(setting);
                }
                Err(e) => eprintln!("cosmic-nightlight: cannot create ramp file: {e}"),
            }
        }
        let _ = self.conn.flush();
    }

    /// Move towards the current target, fading if the change is noticeable.
    fn retarget(&mut self, fade_seconds: f64) {
        let to = self.target();
        if self.current.close_to(&to) && self.fade.is_none() {
            // Small scheduled steps (every 30 s at dusk) are applied directly.
            if self.current != to || self.heads.iter().any(|h| h.size.is_some() && h.applied.is_none()) {
                self.apply(to);
            }
            if self.exit_after_fade {
                self.exit = true;
            }
            return;
        }
        let large = (1e6 / self.current.temp - 1e6 / to.temp).abs() > 10.0
            || (self.current.brightness - to.brightness).abs() > 0.03;
        if !large || fade_seconds <= 0.0 {
            self.fade = None;
            self.apply(to);
            if self.exit_after_fade {
                self.exit = true;
            }
            return;
        }
        self.fade = Some(Fade {
            from: self.current,
            to,
            start: Instant::now(),
            duration: Duration::from_secs_f64(fade_seconds),
        });
        if !self.fade_timer_running {
            self.fade_timer_running = true;
            let _ = self.loop_handle.insert_source(Timer::immediate(), |_, _, app: &mut App| {
                let Some(fade) = app.fade else {
                    app.fade_timer_running = false;
                    return TimeoutAction::Drop;
                };
                let t = (fade.start.elapsed().as_secs_f64() / fade.duration.as_secs_f64()).min(1.0);
                let eased = t * t * (3.0 - 2.0 * t); // smoothstep
                let setting = Setting::lerp_mired(fade.from, fade.to, eased);
                let done = t >= 1.0;
                app.apply(if done { fade.to } else { setting });
                if done {
                    app.fade = None;
                    app.fade_timer_running = false;
                    if app.exit_after_fade {
                        app.exit = true;
                    }
                    TimeoutAction::Drop
                } else {
                    TimeoutAction::ToDuration(Duration::from_millis(33))
                }
            });
        }
    }
}

fn ramp_file(ramp: &[u16]) -> std::io::Result<File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("cosmic-nightlight-ramp-{}", std::process::id()));
    let mut file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path)?;
    let _ = fs::remove_file(&path); // anonymous from here on
    let bytes: Vec<u8> = ramp.iter().flat_map(|v| v.to_ne_bytes()).collect();
    file.write_all(&bytes)?;
    Ok(file)
}

impl Dispatch2<ZwlrGammaControlManagerV1, App> for ManagerData {
    fn event(
        &self,
        _: &mut App,
        _: &ZwlrGammaControlManagerV1,
        _: <ZwlrGammaControlManagerV1 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}

impl Dispatch2<ZwlrGammaControlV1, App> for ControlData {
    fn event(
        &self,
        app: &mut App,
        control: &ZwlrGammaControlV1,
        event: zwlr_gamma_control_v1::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        let Some(head) = app.heads.iter_mut().find(|h| &h.control == control) else { return };
        match event {
            zwlr_gamma_control_v1::Event::GammaSize { size } => {
                head.size = Some(size as usize);
                head.applied = None;
                let current = app.current;
                app.apply(current);
            }
            zwlr_gamma_control_v1::Event::Failed => {
                head.failed = true;
                eprintln!("cosmic-nightlight: gamma control failed for an output");
                if !app.warned_failed {
                    app.warned_failed = true;
                    notify("Night light can't control a display: it doesn't support gamma tables, or another app (e.g. gammastep) is already controlling it.");
                }
            }
            _ => {}
        }
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.add_output(output);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.heads.retain(|h| {
            if h.output == output {
                h.control.destroy();
                false
            } else {
                true
            }
        });
    }
}

delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_dispatch2!(App);

// ---------------------------------------------------------------------------
// Process management / CLI
// ---------------------------------------------------------------------------

fn pid_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join("cosmic-nightlight.pid")
}

fn running_pid() -> Option<u32> {
    let pid: u32 = fs::read_to_string(pid_path()).ok()?.trim().parse().ok()?;
    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    comm.trim().starts_with("cosmic-nightlig").then_some(pid)
}

fn send_signal(sig: &str) -> bool {
    match running_pid() {
        Some(pid) => Command::new("kill")
            .arg(format!("-{sig}"))
            .arg(pid.to_string())
            .status()
            .is_ok_and(|s| s.success()),
        None => false,
    }
}

fn wait_for_exit(pid: u32) {
    for _ in 0..100 {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn notify(msg: &str) {
    let _ = Command::new("notify-send")
        .args(["-a", "Night light", "-i", "weather-clear-night", "-t", "2500", msg])
        .status();
}

fn usage() {
    println!(
        "cosmic-nightlight - night light for COSMIC (hardware gamma)\n\n\
         USAGE:\n  \
         cosmic-nightlight              run, following sunset/sunrise (from config)\n  \
         cosmic-nightlight --temp K     run with a fixed temperature, e.g. --temp 3400\n  \
         cosmic-nightlight toggle       pause/resume the running instance (starts it if needed)\n  \
         cosmic-nightlight reload       re-read the config file\n  \
         cosmic-nightlight stop         fade out and stop\n  \
         cosmic-nightlight restart      stop and start again in the background\n  \
         cosmic-nightlight status       show sun elevation and scheduled setting\n  \
         cosmic-nightlight install-autostart   start automatically when you log in\n\n\
         Config: {}",
        config_path().display()
    );
}

fn spawn_background(args: &[&str]) {
    let exe = std::env::current_exe().expect("current exe");
    let _ = Command::new(exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn bind_manager(globals: &GlobalList, qh: &QueueHandle<App>) -> Option<ZwlrGammaControlManagerV1> {
    globals.bind::<ZwlrGammaControlManagerV1, App, ManagerData>(qh, 1..=1, ManagerData).ok()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut fixed_temp = None;
    match args.first().map(String::as_str) {
        None => {}
        Some("--temp") => match args.get(1).and_then(|v| v.parse::<f64>().ok()) {
            Some(t) => fixed_temp = Some(t.clamp(1700.0, 25000.0)),
            None => return usage(),
        },
        Some("toggle") => {
            if !send_signal("USR1") {
                spawn_background(&[]);
            }
            return;
        }
        Some("reload") => {
            if !send_signal("HUP") {
                println!("not running");
            }
            return;
        }
        Some("stop") => {
            match running_pid() {
                Some(pid) => {
                    send_signal("TERM");
                    wait_for_exit(pid);
                }
                None => println!("not running"),
            }
            return;
        }
        Some("restart") => {
            if let Some(pid) = running_pid() {
                send_signal("TERM");
                wait_for_exit(pid);
            }
            spawn_background(&[]);
            return;
        }
        Some("install-autostart") => {
            let exe = std::env::current_exe().expect("current exe");
            let dir = config_path().parent().unwrap().parent().unwrap().join("autostart");
            let _ = fs::create_dir_all(&dir);
            let path = dir.join("cosmic-nightlight.desktop");
            let entry = format!(
                "[Desktop Entry]\nType=Application\nName=Night light\n\
                 Comment=Warm screen colours after sunset (cosmic-nightlight)\n\
                 Exec={}\nIcon=weather-clear-night\nNoDisplay=true\n",
                exe.display()
            );
            match fs::write(&path, entry) {
                Ok(()) => println!("wrote {}", path.display()),
                Err(e) => eprintln!("cannot write {}: {e}", path.display()),
            }
            return;
        }
        Some("status") => {
            let cfg = load_config();
            let (_, elev) = day_fraction(&cfg, now_unix());
            let s = scheduled_setting(&cfg, now_unix());
            let g = white_point_gains(s.temp, cfg.temp_day);
            println!("sun elevation : {elev:.1} deg");
            println!("scheduled     : {:.0} K, brightness {:.2}", s.temp, s.brightness);
            println!("channel gains : R {:.3}  G {:.3}  B {:.3} (linear light)", g[0], g[1], g[2]);
            match running_pid() {
                Some(pid) => println!("running       : yes (pid {pid})"),
                None => println!("running       : no"),
            }
            println!("config        : {}", config_path().display());
            return;
        }
        Some(_) => return usage(),
    }

    if let Some(pid) = running_pid() {
        eprintln!("cosmic-nightlight is already running (pid {pid}). Use `cosmic-nightlight restart`.");
        std::process::exit(1);
    }

    let conn = Connection::connect_to_env().expect("cannot connect to Wayland display");
    let (globals, event_queue) = registry_queue_init(&conn).expect("registry init");
    let qh = event_queue.handle();

    let Some(manager) = bind_manager(&globals, &qh) else {
        eprintln!(
            "cosmic-nightlight: the compositor does not support wlr-gamma-control.\n\
             Stock cosmic-comp doesn't have it yet; install the patched build \
             (see https://github.com/jasonrsmith/cosmic-nightlight)."
        );
        notify("Night light unavailable: the running compositor has no gamma control (COSMIC was probably updated). See https://github.com/jasonrsmith/cosmic-nightlight");
        std::process::exit(2);
    };

    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();

    let cfg = load_config();
    let neutral = Setting { temp: cfg.temp_day, brightness: 1.0 };
    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        manager,
        conn: conn.clone(),
        qh,
        loop_handle: handle.clone(),
        heads: Vec::new(),
        cfg,
        fixed_temp,
        paused: false,
        current: neutral,
        fade: None,
        fade_timer_running: false,
        warned_failed: false,
        exit_after_fade: false,
        exit: false,
    };

    WaylandSource::new(conn, event_queue).insert(handle.clone()).expect("wayland source");

    let signals = Signals::new(&[Signal::SIGUSR1, Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP])
        .expect("signals");
    handle
        .insert_source(signals, |ev, _, app: &mut App| {
            let fade = app.cfg.fade_seconds;
            match ev.signal() {
                Signal::SIGUSR1 => {
                    app.paused = !app.paused;
                    notify(if app.paused { "Night light paused" } else { "Night light on" });
                    app.retarget(fade);
                }
                Signal::SIGHUP => {
                    app.cfg = load_config();
                    for head in &mut app.heads {
                        head.applied = None;
                    }
                    app.retarget(fade);
                }
                _ => {
                    // Fade back to neutral, then exit (the compositor restores
                    // the original gamma when our controls are destroyed).
                    app.exit_after_fade = true;
                    app.retarget(fade.min(0.6));
                }
            }
        })
        .expect("signal source");

    // Startup: wait briefly for gamma_size events, then fade in.
    handle
        .insert_source(Timer::from_duration(Duration::from_millis(250)), |_, _, app: &mut App| {
            let fade = app.cfg.fade_seconds.max(1.0);
            app.retarget(fade);
            TimeoutAction::Drop
        })
        .expect("timer");
    // Follow the schedule.
    handle
        .insert_source(Timer::from_duration(Duration::from_secs(30)), |_, _, app: &mut App| {
            if app.fade.is_none() {
                let fade = app.cfg.fade_seconds;
                app.retarget(fade);
            }
            TimeoutAction::ToDuration(Duration::from_secs(30))
        })
        .expect("timer");

    let _ = fs::write(pid_path(), std::process::id().to_string());

    while !app.exit {
        if let Err(e) = event_loop.dispatch(None, &mut app) {
            eprintln!("cosmic-nightlight: event loop error: {e}");
            break;
        }
    }

    for head in &app.heads {
        head.control.destroy();
    }
    let _ = app.conn.flush();

    if running_pid() == Some(std::process::id()) {
        let _ = fs::remove_file(pid_path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_is_identity() {
        let ramp = build_ramp(Setting { temp: 6500.0, brightness: 1.0 }, 6500.0, 256);
        for c in 0..3 {
            for i in 0..256 {
                let expect = (i as f64 / 255.0 * 65535.0).round() as i32;
                assert!((ramp[c * 256 + i] as i32 - expect).abs() <= 1);
            }
        }
    }

    #[test]
    fn night_is_warm_and_blacks_stay_black() {
        let g = white_point_gains(3700.0, 6500.0);
        assert!((g[0] - 1.0).abs() < 1e-9, "red should be the max channel: {g:?}");
        assert!(g[0] > g[1] && g[1] > g[2], "{g:?}");
        let ramp = build_ramp(Setting { temp: 3700.0, brightness: 1.0 }, 6500.0, 1024);
        assert_eq!(ramp[0], 0);
        assert_eq!(ramp[1024], 0);
        assert_eq!(ramp[2048], 0);
        assert_eq!(ramp[1023], 65535);
        assert!(ramp[2047] < ramp[1023] && ramp[3071] < ramp[2047]);
    }

    #[test]
    fn ramps_are_monotonic() {
        for temp in [1700.0, 2500.0, 3700.0, 4500.0, 6500.0] {
            let ramp = build_ramp(Setting { temp, brightness: 0.8 }, 6500.0, 1024);
            for c in 0..3 {
                let ch = &ramp[c * 1024..(c + 1) * 1024];
                assert!(ch.windows(2).all(|w| w[0] <= w[1]), "temp {temp} channel {c}");
            }
        }
    }

    #[test]
    fn planckian_locus_known_points() {
        // Illuminant A (2856 K): x=0.4476, y=0.4074
        let (x, y) = planckian_xy(2856.0);
        assert!((x - 0.4476).abs() < 0.002 && (y - 0.4074).abs() < 0.002, "{x} {y}");
    }

    #[test]
    fn mired_lerp_endpoints() {
        let a = Setting { temp: 3700.0, brightness: 0.8 };
        let b = Setting { temp: 6500.0, brightness: 1.0 };
        assert!(Setting::lerp_mired(a, b, 0.0).close_to(&a));
        assert!(Setting::lerp_mired(a, b, 1.0).close_to(&b));
    }

    #[test]
    fn sun_is_up_at_noon_greenwich_solstice() {
        // 2026-06-21 12:00 UTC: expected ~ 90 - 51.48 + 23.44 = 61.96 deg
        let e = solar_elevation(1_782_043_200.0, 51.48, 0.0);
        assert!((e - 61.96).abs() < 0.5, "elevation {e}");
    }

    #[test]
    fn sun_is_down_at_night() {
        // 2026-06-22 00:00 UTC at Greenwich: ~ -(90 - 51.48 - 23.44) = -15.08 deg
        let e = solar_elevation(1_782_086_400.0, 51.48, 0.0);
        assert!((e + 15.08).abs() < 0.5, "elevation {e}");
    }
}
