//! Spoty — a Spotify client for the TrimUI Brick / Brick Pro.

mod audio;
mod config;
mod gfx;
mod local;
mod logger;
mod platform;
mod spotify;
mod ui;
mod update;

use std::path::PathBuf;
use std::time::Duration;

fn arg_value(args: &[String], flag: &str) -> Option<PathBuf> {
    let i = args.iter().position(|a| a == flag)?;
    Some(PathBuf::from(args.get(i + 1).cloned().unwrap_or_else(|| ".".into())))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
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

    let screen = match platform::open_screen(&cfg) {
        Ok(s) => s,
        Err(e) => {
            log::error!("display: {e}");
            std::process::exit(1);
        }
    };
    let fonts = gfx::Fonts::load(&paths.fonts_dir());
    let (tx, rx) = std::sync::mpsc::channel();
    platform::start_input(&cfg, tx.clone());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("spotify")
        .enable_all()
        .build()
        .expect("tokio runtime");
    let cmd = spotify::spawn(&rt, cfg.clone(), paths.clone(), tx.clone());

    let exit = ui::run(screen, fonts, rx, tx, cmd, cfg, paths.clone());

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
