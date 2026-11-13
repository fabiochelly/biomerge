#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod cli;
mod folder_view;
mod formats;
mod fonts;
mod git;
mod icons;
mod instance;
mod merge_view;
mod palette;
mod panes;
mod rules_editor;
mod scroll;
mod sync;
mod text_view;
mod theme;
mod widgets;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

/// A panic is otherwise only visible in a terminal, if there is one.
fn log_panics() {
    let log = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".local/share/biomerge/crash.log"));
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(log) = &log {
            let backtrace = std::backtrace::Backtrace::force_capture();
            let date = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            let report = format!("--- {date}\n{info}\n{backtrace}\n");
            if let Some(parent) = log.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let file = std::fs::OpenOptions::new().create(true).append(true).open(log);
            if let Ok(mut file) = file {
                let _ = std::io::Write::write_all(&mut file, report.as_bytes());
            }
        }
        previous(info);
    }));
}

fn main() -> eframe::Result {
    log_panics();
    let mut args = match cli::parse(std::env::args_os().skip(1)) {
        Ok(cli::Parsed::Run(args)) => args,
        Ok(cli::Parsed::Help) => {
            print!("{}", cli::HELP);
            return Ok(());
        }
        Ok(cli::Parsed::Version) => {
            println!("biomerge {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Err(error) => {
            eprintln!("biomerge : {error}\n");
            eprint!("{}", cli::HELP);
            std::process::exit(2);
        }
    };
    // The window may run in another folder: relative paths (git gives some, relative to the top
    // of the repository where it runs the tool) are made whole.
    let absolute = |path: &mut PathBuf| {
        if let Ok(whole) = std::path::absolute(&*path) {
            *path = whole;
        }
    };
    if let Some(paths) = &mut args.request.paths {
        paths.iter_mut().for_each(absolute);
    }
    if let Some(merge) = &mut args.request.merge {
        merge.paths_mut().into_iter().for_each(absolute);
    }
    args.request.head.iter_mut().chain(&mut args.request.single).for_each(absolute);
    // A merge tool tells git whether the file is resolved through its exit code. So does a
    // waiting launch with one path, if the window finds the file in conflict and merges it;
    // whatever else that path opens ends with 0.
    let merging = args.request.merge.is_some() || (args.request.wait && args.request.single.is_some());

    let socket = instance::default_socket_path();
    let mut listener = None;
    if !args.new_window {
        match instance::hand_over(&socket, &args.request) {
            Some(instance::Handed::Refused(message)) => {
                eprintln!("biomerge : {message}");
                std::process::exit(1);
            }
            Some(handed) => {
                instance::raise();
                let code = handed.wait();
                if merging {
                    // A window that went away without a word saved nothing.
                    std::process::exit(code.unwrap_or(1));
                }
                return Ok(());
            }
            // Nobody took it: this launch is the window, and listens for the next ones.
            None => listener = instance::Listener::bind(&socket),
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("biomerge")
            .with_title("BioMerge")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([640.0, 360.0])
            // An alpha channel from the start, whatever the theme: Verre's glass needs it, the
            // other themes paint every pixel solid, and the theme can change without a restart.
            .with_transparent(true),
        ..Default::default()
    };
    // Unresolved until the merge tab this launch opened says otherwise.
    let outcome = Arc::new(AtomicI32::new(1));
    let own = outcome.clone();
    let result = eframe::run_native(
        "biomerge",
        options,
        Box::new(move |creation| {
            let mut app = app::App::new(&creation.egui_ctx, creation.storage, Some(args.request));
            if merging && !app.wait_on_first_tab(instance::Waiter::local(own.clone())) {
                // One path that opened the comparison dialog: nothing to resolve.
                own.store(0, Ordering::Relaxed);
            }
            if let Some(listener) = listener.take() {
                app.listen(&creation.egui_ctx, listener);
            }
            Ok(Box::new(app))
        }),
    );
    if merging {
        std::process::exit(outcome.load(Ordering::Relaxed));
    }
    result
}
