// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 pippin contributors. Protocol knowledge derived from
// iShareScreen (https://github.com/renegadelink/iShareScreen), AGPL-3.0-or-later.

//! pippin: a native client for macOS Screen Sharing's high-performance
//! (HEVC) mode.

mod audio;
mod clipboard;
mod credentials;
mod errors;
mod headless;
#[cfg(test)]
mod interop_tests;
mod proto;
mod rtp;
mod session;
mod snapshot;
mod ui;
mod video;

use std::io::{BufRead, IsTerminal};
use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use clap::Parser;

use proto::handshake::SessionMode;
use rtp::assembler::HoldMode;
use video::decoder::DecoderKind;

#[derive(Parser)]
#[command(version, about = "Connect to a Mac's built-in Screen Sharing in high-performance mode")]
struct Cli {
    /// Host name or IPv4 address of the Mac. Asked for in the connect
    /// dialog when missing and there is no terminal.
    host: Option<String>,
    /// macOS account name.
    #[arg(short, long, env = "PIPPIN_USER")]
    user: Option<String>,
    /// Always show the connect dialog, even with a terminal or a stored
    /// password. It's shown anyway when something is missing and stdin
    /// isn't a terminal (e.g. launched from a .desktop file).
    #[arg(long)]
    dialog: bool,
    /// Run without a window, for scripted tests: decode the video and show
    /// nothing. --size sets the Mac display in points. No clipboard sync, no
    /// sound, and no reconnect. PIPPIN_CONTROL_FIFO sends input and takes
    /// snapshots.
    #[arg(long, conflicts_with = "dialog")]
    headless: bool,
    /// Account password. Otherwise taken from the keyring or prompted for.
    #[arg(long, env = "PIPPIN_PASSWORD", hide_env_values = true)]
    password: Option<String>,
    /// Read the password from the first line of stdin.
    #[arg(long, conflicts_with = "password")]
    password_stdin: bool,
    /// Save the password to the keyring once it authenticates, without
    /// asking. Otherwise only a prompted password is offered for saving.
    #[arg(long)]
    save_password: bool,
    /// Delete the keyring entry for this account and exit.
    #[arg(long)]
    forget_password: bool,
    #[arg(long, default_value_t = 5900)]
    port: u16,
    /// Share the console user's screen, or open a separate login session.
    #[arg(long, value_enum, default_value_t = SessionMode::Share)]
    session: SessionMode,
    /// Initial window size in logical pixels [default: 1600x900]. With
    /// --headless, the size of the Mac display in points, e.g. 2560x1440.
    #[arg(long, value_parser = parse_size)]
    size: Option<(u32, u32)>,
    /// Backing scale for the virtual display (1 = flat, 2 = Retina).
    /// Defaults to 2 on HiDPI screens, else 1, and to 1 with --headless.
    #[arg(long)]
    scale: Option<f64>,
    /// Largest virtual display to ask the Mac for, in pixels, e.g.
    /// 5120x2880. Defaults to the size of the largest monitor. It holds for
    /// the whole session. The Mac can still choose a smaller one. Not with
    /// --headless, where --size sets the exact display.
    #[arg(long, value_parser = parse_size, conflicts_with = "headless")]
    max_resolution: Option<(u32, u32)>,
    /// Fraction of the window's pixels the Mac renders and streams; the
    /// picture is scaled up locally. 0.5 on a Retina screen streams a quarter
    /// of the pixels: less bandwidth and smaller keyframes for slow links, at
    /// the cost of softer text. The Mac's layout is unchanged. macOS only
    /// renders whole scales, so 0.5 needs a Retina (2x) window.
    #[arg(long, default_value_t = 1.0)]
    render_scale: f64,
    /// Hold frames with missing packets for NACK retransmissions. auto stops
    /// holding when the host doesn't retransmit.
    #[arg(long, value_enum, default_value_t = HoldMode::Auto)]
    hold: HoldMode,
    /// What to show between a loss and the next keyframe: idr freezes the
    /// picture (drops everything until the keyframe); conceal keeps decoding
    /// whatever arrived, so the picture keeps moving but is smeared.
    #[arg(long, value_enum, default_value_t = session::AfterLoss::Idr)]
    after_loss: session::AfterLoss,
    /// Append one JSON line of link and recovery metrics per second to this
    /// file, for experiments.
    #[arg(long)]
    stats_json: Option<PathBuf>,
    /// Keep the remote resolution fixed instead of following the window.
    #[arg(long)]
    no_auto_resize: bool,
    /// Stream the Mac's own main display instead of a virtual display for
    /// pippin. Use it when the Mac's main display is a virtual display, as on
    /// a Mac without a monitor: there, apps draw nowhere while pippin has its
    /// own display. The window shows the Mac's display scaled to fit, and
    /// never resizes it. The Mac's own screen shows the session too. --scale,
    /// --max-resolution, --render-scale, and --size with --headless set
    /// pippin's virtual display, so pippin refuses them with this flag.
    #[arg(long, conflicts_with_all = ["scale", "max_resolution", "render_scale"])]
    no_virtual_display: bool,
    /// Send Ctrl as Command and Super as Control.
    #[arg(long)]
    swap_ctrl_cmd: bool,
    /// Video decoder. auto tries CUDA, then VAAPI, then software, and falls
    /// back further if a hardware decoder can't handle the stream.
    #[arg(long, value_enum, default_value_t = DecoderKind::Auto)]
    decoder: DecoderKind,
    /// Same as --decoder software.
    #[arg(long)]
    no_hw: bool,
    /// DRM render node for VAAPI decoding, e.g. /dev/dri/renderD129.
    /// Default: FFmpeg's choice.
    #[arg(long, env = "PIPPIN_VAAPI_DEVICE")]
    vaapi_device: Option<String>,
    /// Software decode threads. Pictures are split by CTU row (wavefront),
    /// so more threads add no latency; gains level off around 4-5.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..=16))]
    decode_threads: u32,
    /// Show each frame's tiles together. Prevents horizontal tearing while
    /// windows move, but holds early tiles until the last one is decoded.
    #[arg(long)]
    sync_tiles: bool,
    /// Don't play the Mac's sound (and ask it not to send any).
    #[arg(long)]
    no_audio: bool,
    /// Skip the preliminary connection that wakes the host's daemon.
    #[arg(long)]
    no_warmup: bool,
    /// Exit when the connection drops instead of reconnecting. This also
    /// makes `restart` on the PIPPIN_CONTROL_FIFO exit.
    #[arg(long)]
    no_reconnect: bool,
    /// More logging; repeat for debug output.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

/// The window size, and the Mac display of --headless, without --size.
const DEFAULT_SIZE: (u32, u32) = (1600, 900);

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once(['x', 'X']).ok_or("expected WIDTHxHEIGHT")?;
    let w = w.trim().parse().map_err(|_| "bad width")?;
    let h = h.trim().parse().map_err(|_| "bad height")?;
    if w < 320 || h < 200 {
        return Err("size too small".into());
    }
    Ok((w, h))
}

fn resolve_ipv4(host: &str, port: u16) -> Result<Ipv4Addr> {
    (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolving {host}"))?
        .find_map(|a| match a {
            SocketAddr::V4(v4) => Some(*v4.ip()),
            SocketAddr::V6(_) => None,
        })
        .with_context(|| format!("{host} has no IPv4 address"))
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let headless = cli.headless;
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // Session failures arrive already classified; full chains for
            // everything are at debug level.
            tracing::debug!("{e:?}");
            eprintln!("pippin: {e:#}");
            if !headless && !std::io::stdin().is_terminal() {
                // Launched from a desktop entry: stderr goes nowhere, so try
                // a notification. Missing notify-send is fine.
                let _ = std::process::Command::new("notify-send")
                    .args(["pippin", &format!("{e:#}")])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            std::process::ExitCode::FAILURE
        }
    }
}

/// Refuses the options that cannot work together, beyond what clap checks.
fn check(cli: &Cli) -> Result<()> {
    if let Some(s) = cli.scale
        && !(1.0..=3.0).contains(&s) {
            bail!("--scale must be between 1 and 3");
        }
    if cli.render_scale != 1.0 && cli.render_scale != 0.5 {
        bail!("--render-scale must be 1 or 0.5 (macOS only renders at whole scales)");
    }
    if cli.headless && cli.no_virtual_display && cli.size.is_some() {
        bail!("--size with --headless sets pippin's virtual display, which --no-virtual-display turns off");
    }
    Ok(())
}

fn run(cli: Cli) -> Result<()> {
    let default_filter = match cli.verbose {
        0 => "pippin=info,wgpu=error,naga=error",
        1 => "pippin=debug,wgpu=warn",
        _ => "pippin=trace,wgpu=info",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter)),
        )
        .with_target(false)
        .init();
    video::decoder::set_log_level(cli.verbose > 0);

    check(&cli)?;
    let store: Arc<dyn credentials::Store> = Arc::new(credentials::Keyring);
    if cli.forget_password {
        let (Some(host), Some(user)) = (&cli.host, &cli.user) else {
            bail!("--forget-password needs the host and --user");
        };
        let account = credentials::account(user, host, cli.port);
        match store.delete(&account) {
            Ok(true) => println!("removed stored password for {account}"),
            Ok(false) => println!("no stored password for {account}"),
            Err(e) => return Err(e.context("keyring unavailable")),
        }
        return Ok(());
    }
    let tty = std::io::stdin().is_terminal();
    let target = cli.host.clone().zip(cli.user.clone());
    let mut found = None;
    if let Some((host, user)) = &target
        && !cli.dialog
    {
        let mut stdin = cli.password_stdin.then(|| std::io::stdin().lock());
        let account = credentials::account(user, host, cli.port);
        found = credentials::lookup(
            cli.password.clone(),
            stdin.as_mut().map(|s| s as &mut dyn BufRead),
            store.as_ref(),
            &account,
        )?;
    }
    // Headless needs no display server, and opens no connection to one.
    let mut event_loop = if cli.headless { None } else { Some(ui::event_loop()?) };
    let mut save = cli.save_password;
    let mode = credentials::choose(cli.dialog, target.is_some(), found.is_some(), tty);
    let (host, user, password, source) = match (mode, target, found) {
        (credentials::Mode::Direct, Some((h, u)), Some((p, s))) => (h, u, p, s),
        (credentials::Mode::Terminal, Some((h, u)), _) => {
            let p = rpassword::prompt_password(format!("Password for {u}@{h}: "))?;
            // Asked up front, while the user is at the terminal; the password
            // is only stored once the host accepts it.
            if !cli.save_password
                && credentials::available(store.as_ref(), &credentials::account(&u, &h, cli.port))
                && credentials::confirm_save(&mut std::io::stdin().lock())
            {
                save = true;
            }
            (h, u, p, credentials::Source::Prompt)
        }
        (credentials::Mode::Dialog, ..) => match event_loop.as_mut() {
            Some(event_loop) => return dialog_loop(event_loop, &cli, &store),
            None => bail!(
                "--headless needs the host, --user and a password (--password, --password-stdin, \
                 PIPPIN_PASSWORD or the keyring)"
            ),
        },
        _ => bail!("the host and --user are required (or use --dialog)"),
    };
    let viewer = event_loop.as_mut().map_or(Viewer::Headless, Viewer::Window);
    connect(viewer, &cli, &store, &host, &user, password, source, save, false).map(drop)
}

/// Where a session shows its picture.
enum Viewer<'a> {
    Window(&'a mut winit::event_loop::EventLoop<ui::Tagged>),
    Headless,
}

/// Shows the connect dialog until a session runs or the user cancels; any
/// failure brings the dialog back with the error under the fields.
fn dialog_loop(
    event_loop: &mut winit::event_loop::EventLoop<ui::Tagged>,
    cli: &Cli,
    store: &Arc<dyn credentials::Store>,
) -> Result<()> {
    let mut fields = ui::dialog::Fields {
        host: cli.host.clone().unwrap_or_default(),
        user: cli.user.clone().unwrap_or_default(),
        password: cli.password.clone().unwrap_or_default(),
        save: cli.save_password,
    };
    let mut error = None;
    let port = cli.port;
    // Probed once: the keyring doesn't come and go while the dialog is open.
    let keyring = credentials::available(store.as_ref(), &credentials::account(&fields.user, &fields.host, port));
    if !keyring {
        fields.save = false;
    }
    let stored = |host: &str, user: &str| match store.get(&credentials::account(user, host, port)) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("keyring unavailable: {e:#}");
            None
        }
    };
    loop {
        let has_saved = {
            let store = store.clone();
            move |host: &str, user: &str| matches!(store.get(&credentials::account(user, host, port)), Ok(Some(_)))
        };
        let Some(f) = ui::dialog::run(event_loop, fields.clone(), error.take(), keyring, has_saved)? else {
            return Ok(());
        };
        let Some((password, source)) = credentials::dialog_password(f.password.clone(), || stored(&f.host, &f.user))
        else {
            error = Some(format!("No saved password for {}@{}; enter one.", f.user, f.host));
            fields = f;
            continue;
        };
        match connect(Viewer::Window(event_loop), cli, store, &f.host, &f.user, password, source, f.save, true)? {
            Outcome::Done => return Ok(()),
            Outcome::Retry { msg, wrong_password } => {
                error = Some(msg);
                // Keep a password that may well be right for another try.
                let password = if wrong_password { String::new() } else { f.password.clone() };
                fields = ui::dialog::Fields { password, ..f };
            }
        }
    }
}

enum Outcome {
    Done,
    /// The attempt failed; ask again with this message.
    Retry { msg: String, wrong_password: bool },
}

/// Runs one session. For a dialog password every failure comes back as
/// `Retry` so the dialog can show it and ask again.
#[allow(clippy::too_many_arguments)]
fn connect(
    viewer: Viewer,
    cli: &Cli,
    store: &Arc<dyn credentials::Store>,
    host_name: &str,
    user: &str,
    password: String,
    source: credentials::Source,
    save: bool,
    from_dialog: bool,
) -> Result<Outcome> {
    let host = match resolve_ipv4(host_name, cli.port) {
        Ok(h) => h,
        Err(e) if from_dialog => return Ok(Outcome::Retry { msg: format!("{e:#}"), wrong_password: false }),
        Err(e) => return Err(e),
    };
    let account = credentials::account(user, host_name, cli.port);
    let rejected = Arc::new(AtomicBool::new(false));
    // Set on the first accepted login. The hook runs for every reconnect
    // too, so saving happens at most once per password (one connect() call
    // is one password; a new one typed into the dialog after a rejection
    // gets its own chance).
    let authed = Arc::new(AtomicBool::new(false));
    let on_auth: session::AuthHook = {
        let (store, account, password) = (store.clone(), account.clone(), password.clone());
        let (rejected, authed) = (rejected.clone(), authed.clone());
        Arc::new(move |ok| {
            rejected.store(!ok, Ordering::SeqCst);
            if ok && authed.swap(true, Ordering::SeqCst) {
                return;
            }
            // Runs on the control thread; never asks here.
            credentials::after_auth(store.as_ref(), &account, &password, source, ok, save, || false);
        })
    };
    // Headless runs on a machine where someone else may be listening.
    let audio = if cli.no_audio || cli.headless {
        None
    } else {
        audio::Player::new()
            .inspect_err(|e| tracing::warn!("no sound: {e:#}"))
            .ok()
    };
    let cfg = session::Config {
        host,
        port: cli.port,
        user: user.to_owned(),
        password: password.clone(),
        session_mode: cli.session,
        decoder: if cli.no_hw { DecoderKind::Software } else { cli.decoder },
        vaapi_device: cli.vaapi_device.clone(),
        decode_threads: cli.decode_threads,
        sync_tiles: cli.sync_tiles,
        warmup: !cli.no_warmup,
        on_auth: Some(on_auth),
        resumed: false,
        op: Arc::new(parking_lot::Mutex::new(session::OperatingPoint {
            render_scale: cli.render_scale,
            // Frame rate, keyframe request kind and LTR are experiment
            // knobs on the control FIFO only; none made a measurable
            // difference on a lossy link, as the Mac doesn't retransmit.
            fps: session::Fps::Sixty,
            recovery: rtp::rtcp::Recovery::FirPli,
            hold: cli.hold,
            ltr: true,
            after_loss: cli.after_loss,
        })),
        stats_json: cli.stats_json.clone(),
        // Debug only: switch operating-point levers mid-session by writing
        // lines such as `fps 30` or `hold off` to this FIFO, or send input.
        control_fifo: std::env::var_os("PIPPIN_CONTROL_FIFO").map(PathBuf::from),
        audio: audio.clone(),
        // The local clipboard belongs to whoever sits at this machine.
        clipboard: !cli.headless,
    };
    let event_loop = match viewer {
        Viewer::Window(event_loop) => event_loop,
        Viewer::Headless => {
            let display = (!cli.no_virtual_display).then(|| (cli.size.unwrap_or(DEFAULT_SIZE), cli.scale.unwrap_or(1.0)));
            return headless::run(cfg, display).map(|()| Outcome::Done);
        }
    };
    let opts = ui::Options {
        title: format!("{host_name} — pippin"),
        host_name: host_name.to_owned(),
        size: cli.size.unwrap_or(DEFAULT_SIZE),
        scale: cli.scale,
        max_resolution: cli.max_resolution,
        auto_resize: !cli.no_auto_resize,
        virtual_display: !cli.no_virtual_display,
        swap_ctrl_cmd: cli.swap_ctrl_cmd,
        reconnect: !cli.no_reconnect,
    };
    let result = ui::run(event_loop, cfg, opts);
    match result {
        Err(e) if from_dialog => {
            tracing::debug!("{e:?}");
            let wrong_password = rejected.load(Ordering::SeqCst);
            let msg = if wrong_password { "Wrong user name or password".into() } else { format!("{e:#}") };
            Ok(Outcome::Retry { msg, wrong_password })
        }
        r => r.map(|()| Outcome::Done),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli> {
        let cli = Cli::try_parse_from([&["pippin", "mac.local", "--user", "alice"], args].concat())?;
        check(&cli)?;
        Ok(cli)
    }

    #[test]
    fn no_virtual_display_refuses_the_options_of_the_virtual_display() {
        for args in [
            &["--render-scale", "0.5"][..],
            &["--render-scale", "1"],
            &["--max-resolution", "5120x2880"],
            &["--scale", "2"],
            &["--headless", "--size", "2560x1440"],
        ] {
            let e = parse(&[&["--no-virtual-display"], args].concat()).err();
            assert!(e.is_some(), "{args:?} was accepted");
        }
    }

    #[test]
    fn headless_refuses_max_resolution_because_size_sets_the_display() {
        assert!(parse(&["--headless", "--max-resolution", "5120x2880"]).is_err());
        assert!(parse(&["--max-resolution", "5120x2880"]).is_ok());
    }

    #[test]
    fn no_virtual_display_takes_the_window_size_and_headless() {
        assert_eq!(parse(&["--no-virtual-display", "--size", "1280x800"]).unwrap().size, Some((1280, 800)));
        assert!(parse(&["--no-virtual-display", "--headless", "--no-auto-resize"]).unwrap().no_virtual_display);
        assert!(parse(&["--headless", "--size", "2560x1440", "--render-scale", "0.5"]).is_ok());
    }
}
