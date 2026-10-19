//! Artifeeds — GPUI desktop shell.
//!
//! The process layout is deliberately simple:
//!
//! ```text
//!   main thread ── GPUI (window, render, key handling)
//!   artifeeds-io × 2 ── tokio: HTTP + parsing
//!   artifeeds-store × 1 ── SQLite writer
//!   artifeeds-config-watch × 1 ── debounced fs watcher
//! ```
//!
//! The UI thread never performs I/O and never waits on a lock: it reads an
//! immutable snapshot and sends commands over a channel.

// The `objc` 0.2 macros still carry `#[cfg(feature = "cargo-clippy")]` guards,
// which rustc reports as an unexpected cfg. Nothing in our code can fix that.
#![allow(unexpected_cfgs)]


mod app;
mod icon;
mod theme;
mod type_scale;
#[cfg(target_os = "macos")]
mod webview;

use anyhow::Context;
use app::App;
use artifeeds_core::config::write_scaffold;
use artifeeds_core::default_config_path;
use artifeeds_core::engine::Engine;
use artifeeds_core::text::human_bytes;
use gpui_kit::component::input::InputState;
use gpui_kit::{
    App as GpuiApp, AppContext as _, Bounds, SharedString, TitlebarOptions, WindowBounds, WindowOptions,
    point, px, size,
};
use std::path::PathBuf;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    // Two sinks: the terminal, for a binary run from a shell, and a file, because
    // an app launched from Finder or the Dock has no terminal at all — and a
    // problem that only happens when the sweep is running is exactly the kind
    // that needs a log afterwards.
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(artifeeds_core::default_data_dir().join("artifeeds.log"))
        .ok();
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_env("RUST_LOG")
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    };
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let terminal = tracing_subscriber::fmt::layer().with_target(false);
    match log_file {
        Some(file) => {
            let to_file = tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file));
            tracing_subscriber::registry()
                .with(filter())
                .with(terminal)
                .with(to_file)
                .init();
        }
        None => {
            tracing_subscriber::registry()
                .with(filter())
                .with(terminal)
                .init();
        }
    }
    tracing::info!("---- Artifeeds {} starting ----", env!("CARGO_PKG_VERSION"));

    let config_path: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);

    if !config_path.exists() {
        write_scaffold(&config_path).context("writing starter config")?;
        tracing::info!("created starter config at {}", config_path.display());
    }

    let started = std::time::Instant::now();
    let engine = Arc::new(Engine::start(config_path.clone()).context("starting feed engine")?);
    let snapshot = engine.snapshot();
    tracing::info!(
        sources = snapshot.stats.sources_total,
        articles = snapshot.stats.mem_articles,
        db = %human_bytes(snapshot.stats.db_bytes),
        elapsed_ms = started.elapsed().as_millis(),
        "engine ready"
    );
    for problem in snapshot.problems.iter().take(5) {
        tracing::warn!("config: {problem}");
    }

    let title = format!("Artifeeds — {}", config_path.display());
    let engine_for_window = Arc::clone(&engine);

    gpui_kit::application().run(move |cx: &mut GpuiApp| {
        // Installs the component library's globals, themes and key bindings.
        gpui_kit::init(cx);
        // `f` and `/` focus a field, so they must be actions: an action consumes
        // the keystroke, while a key listener would let the platform insert the
        // character into the field it just focused.
        cx.bind_keys([
            gpui_kit::KeyBinding::new("/", app::FocusArticleSearch, Some("Artifeeds")),
            gpui_kit::KeyBinding::new("f", app::FocusSidebarFilter, Some("Artifeeds")),
        ]);
        // The app's palette is dark; the library defaults to light.
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        // A bare binary has no bundle to read an icon from.
        icon::install_after_launch();

        let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some(SharedString::from(title.clone())),
                        // Custom toolbar; the system traffic lights float on
                        // top of it (hence the 84 px left inset in the UI).
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(12.), px(12.))),
                    }),
                    window_min_size: Some(size(px(720.), px(420.))),
                    ..Default::default()
                },
                {
                    let engine = Arc::clone(&engine_for_window);
                    move |window, cx| {
                        // The field is created with a window in hand, because
                        // the component needs one to build its text system.
                        let input = cx.new(|cx| {
                            InputState::new(window, cx).placeholder("搜索文章 / 全文历史")
                        });
                        // The sidebar filter is a second input: finding one blog
                        // among 1 400 is a different job from searching articles.
                        let side_input = cx.new(|cx| {
                            InputState::new(window, cx)
                                .placeholder("筛选博客")
                                .clean_on_escape()
                        });
                        // The URL field of the add-a-subscription overlay.
                        let add_input = cx.new(|cx| {
                            InputState::new(window, cx)
                                .placeholder("博客或 feed 地址，回车添加")
                                .clean_on_escape()
                        });
                        let app = cx.new(|cx| {
                            App::new(Arc::clone(&engine), input, side_input, add_input, cx)
                        });
                        // Components read their theme, tooltips and overlays
                        // from `Root`, so the app view lives inside one.
                        cx.new(|cx| gpui_kit::component::Root::new(app, window, cx))
                    }
                },
            )
            .expect("failed to open window");

        let _ = window;
        cx.activate(true);
    });

    // `Application::run` returns when the last window closes.
    engine.shutdown();
    Ok(())
}
