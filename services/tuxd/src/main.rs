//! tuxd — single TUX process for the lightweight MVP.
//!
//! Default: Local Paper closed loop + SQLite persistence + Console dashboard.
//! Optional: `binance` / `okx` venue features. No PostgreSQL/NATS/MinIO required.

mod dashboard_mod;
mod live_feed;
mod pipeline;
mod runtime;

#[cfg(feature = "control-api")]
mod control;

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use rust_decimal::Decimal;
use tux_core::ensure_not_live;
use tux_core::ids::{Environment, MarketContext, Product, Venue};
use tux_core::sim::FillModel;
use tux_core::store::Store;

fn main() {
    setup_tracing();

    let ctx = MarketContext::new(Venue::LocalPaper, Product::Spot, Environment::LocalPaper);
    if let Err(e) = ensure_not_live(ctx.environment) {
        eprintln!("refusing to start: {e}");
        std::process::exit(2);
    }
    tracing::info!(ctx = %ctx, "tuxd starting (Local Paper)");

    let db_path: PathBuf = std::env::var("TUXD_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("data/tux.db"));
    let store = match Store::open(&db_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("open store {}: {e}", db_path.display());
            std::process::exit(1);
        }
    };

    let fill_model = FillModel::immediate();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    runtime.block_on(async move {
        let db_label = db_path.display().to_string();
        let rt = std::sync::Arc::new(runtime::RuntimeState::new(
            tux_core::domain::Instrument::spot(
                "SOL-USDT",
                Venue::LocalPaper,
                "SOL",
                "USDT",
                d("0.01"),
                d("0.001"),
            ),
            tux_core::ids::AccountId::from("paper_acc"),
            tux_core::ids::StrategyInstanceId::from("oneshot"),
            store.clone(),
            tux_core::sim::PaperEngine::new(fill_model.clone()),
            {
                let mut r = tux_core::risk::RiskEngine::new_safe_default();
                r.limits.instrument_whitelist =
                    vec![tux_core::domain::InstrumentId::new("SOL-USDT")];
                r.limits.max_order_notional = Some(d("5000"));
                r.kill_switch.release();
                r.set_feeds_connected(true);
                r
            },
        ));

        #[cfg(feature = "control-api")]
        let serve_handle = {
            let state = control::AppState { rt: rt.clone() };
            let want_serve = std::env::var("TUXD_SERVE").ok().as_deref() == Some("1")
                || std::env::var("TUXD_HOLD").ok().as_deref() == Some("1");
            if want_serve {
                let port = std::env::var("TUXD_PORT").unwrap_or_else(|_| "8080".into());
                println!("Dashboard: http://127.0.0.1:{port}/");
                live_feed::spawn_price_sampler(rt.clone(), "SOLUSDT");
                Some(tokio::spawn(async move {
                    if let Err(e) = serve(state).await {
                        eprintln!("control-api failed: {e}");
                    }
                }))
            } else {
                drop(state);
                None
            }
        };

        match pipeline::run_paper_demo(&rt, fill_model, &db_label).await {
            Ok(report) => {
                println!("\n=== Paper demo complete ===");
                println!("{report}");
            }
            Err(e) => {
                eprintln!("pipeline failed: {e}");
                std::process::exit(1);
            }
        }

        #[cfg(feature = "control-api")]
        {
            if let Some(h) = serve_handle {
                if h.is_finished() {
                    // one-shot placeholder
                } else if std::env::var("TUXD_SERVE").ok().as_deref() == Some("1")
                    || std::env::var("TUXD_HOLD").ok().as_deref() == Some("1")
                {
                    let _ = h.await;
                } else {
                    // one-shot: keep dashboard alive a moment
                    let port = std::env::var("TUXD_PORT").unwrap_or_else(|_| "8080".into());
                    println!(
                        "Dashboard: http://127.0.0.1:{port}/  (use TUXD_HOLD=1 to keep running)"
                    );
                }
            }
        }
    });

    std::process::exit(0);
}

#[cfg(feature = "control-api")]
async fn serve(state: control::AppState) -> anyhow::Result<()> {
    let port = std::env::var("TUXD_PORT").unwrap_or_else(|_| "8080".into());
    let bind = std::env::var("TUXD_BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let addr: std::net::SocketAddr = format!("{bind}:{port}").parse()?;
    let app = control::router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("tuxd console on http://127.0.0.1:{port}/");
    axum::serve(listener, app).await?;
    Ok(())
}

fn setup_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();
}

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).expect("decimal")
}
