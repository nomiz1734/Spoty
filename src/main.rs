//! Spoty — a Spotify client for the TrimUI Brick / Brick Pro.

mod audio;
mod background;
mod config;
mod download;
mod gfx;
mod led;
mod local;
mod logger;
mod net;
mod platform;
mod spotify;
mod ui;
mod update;
mod wifi_transfer;

use std::path::PathBuf;
use std::time::Duration;

fn arg_value(args: &[String], flag: &str) -> Option<PathBuf> {
    let i = args.iter().position(|a| a == flag)?;
    Some(PathBuf::from(args.get(i + 1).cloned().unwrap_or_else(|| ".".into())))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--attach") {
        // launch.sh, standing in for the app in the system launcher.
        std::process::exit(background::attach(args.iter().any(|a| a == "--wait")));
    }
    let paths = config::Paths::detect();

    // Developer helpers: render every screen to PNG, or write the launcher icon.
    if let Some(dir) = arg_value(&args, "--screenshots") {
        let fonts = gfx::Fonts::load(&paths.fonts_dir());
        ui::demo::screenshots(fonts, &dir);
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--decode-test") {
        // Decodes files end to end and prints what the player would see.
        for f in &args[i + 1..] {
            local::selftest(std::path::Path::new(f));
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--play-test") {
        // Plays files through the real output (ALSA on the device) and prints events.
        let cfg = config::Config::load(&paths);
        local::play_selftest(&args[i + 1..], &cfg);
        return;
    }
    if args.iter().any(|a| a == "--home-test") {
        let cfg = config::Config::load(&paths);
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(spotify::home_selftest(cfg, paths));
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--slskd-test") {
        // Searches the download server and prints the results the UI would list.
        let cfg = config::Config::load(&paths);
        let query = args[i + 1..].join(" ");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(download::search_selftest(&cfg, &query));
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--slskd-get") {
        // Downloads the best match into music_dir, printing both stages.
        let cfg = config::Config::load(&paths);
        let query = args[i + 1..].join(" ");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(download::download_selftest(&cfg, &query));
        return;
    }
    if args.iter().any(|a| a == "--led-test") {
        // Shows what the LED firmware offers and runs a visible colour test.
        led::selftest();
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--update-check") {
        // Fetches a manifest the way the app does (HTTPS + redirects) and reports.
        let url = args.get(i + 1).cloned().unwrap_or_default();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        match rt.block_on(update::check(&url)) {
            Ok(Some(info)) => println!("update available: {info:?}"),
            Ok(None) => println!("up to date ({})", update::current_version()),
            Err(e) => println!("check failed: {e}"),
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--install-test") {
        // Installs a package into a scratch app folder: spoty --install-test PKG DIR
        let (pkg, dir) = (PathBuf::from(&args[i + 1]), PathBuf::from(&args[i + 2]));
        let r = update::install_files(&pkg, &dir, &dir.join("data"), "test");
        println!("install: {r:?}");
        return;
    }
    if let Some(dir) = arg_value(&args, "--scan-test") {
        local::scan_selftest(&dir);
        return;
    }
    if let Some(file) = arg_value(&args, "--icon") {
        ui::demo::write_icon(&file, 300);
        return;
    }

    logger::init(&paths.log_file());
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
        log::logger().flush();
    }));
    log::info!(
        "Spoty {} starting (app dir {})",
        env!("CARGO_PKG_VERSION"),
        paths.app_dir.display()
    );
    let cfg = config::Config::load(&paths);

    #[cfg(unix)]
    if std::env::var_os("SPOTY_DETACH").is_some() {
        // Started by launch.sh to outlive it: keep playing after the launcher
        // takes the screen back, whatever it does to its own process group.
        unsafe { libc::setsid() };
    }
    let screen = match platform::Display::open(&cfg) {
        Ok(s) => s,
        Err(e) => {
            log::error!("display: {e}");
            std::process::exit(1);
        }
    };
    let fonts = gfx::Fonts::load(&paths.fonts_dir());
    let (tx, rx) = std::sync::mpsc::channel();
    // Launchers asking to put Spoty back on screen (see background.rs).
    background::listen(tx.clone());
    platform::start_input(&cfg, tx.clone());
    led::start(&cfg);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("spotify")
        .enable_all()
        .build()
        .expect("tokio runtime");
    let cmd = spotify::spawn(&rt, cfg.clone(), paths.clone(), tx.clone());
    let dl = download::spawn(rt.handle(), &cfg, tx.clone());

    let exit = ui::run(screen, fonts, rx, tx, cmd, dl, rt.handle().clone(), cfg, paths.clone());

    led::shutdown();
    rt.shutdown_timeout(Duration::from_secs(1));
    log::logger().flush();
    if exit == ui::Exit::Restart {
        log::info!("restarting into the new version");
        log::logger().flush();
        std::process::exit(update::RESTART_EXIT_CODE);
    }
    log::info!("bye");
    log::logger().flush();
}
