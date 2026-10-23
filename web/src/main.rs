//! snens-web — веб-клиент как отдельный бинарь.
//!
//! Раньше прод-клиент держался на `vite` (dev-сервер в systemd-юните): он отдавал
//! `dist/` и проксировал `/api`, подставляя Bearer. Это работало, но требовало
//! node/npx в поставке, пересборки на месте и не давало скачать клиент одним
//! файлом. Здесь то же самое на Rust: статика из каталога рядом с бинарём и
//! прокси к ядру.
//!
//! Слои: `cli` — что задаёт человек, `static_files` — отдача бандла,
//! `proxy` — `/api` → ядро. `main` только связывает их и держит процесс.

mod cli;
mod proxy;
mod static_files;

use axum::{
    Router,
    routing::{any, get},
};
use std::{sync::Arc, time::Duration};

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match cli::Args::parse(&argv) {
        Ok(Some(a)) => a,
        Ok(None) => {
            print!("{}", cli::HELP);
            return;
        }
        Err(e) => {
            eprintln!("snens-web: {e}\n\n{}", cli::HELP);
            std::process::exit(2);
        }
    };

    // Клиент без `index.html` — это не «пустая страница», а собранная не туда
    // поставка. Говорим сразу и с путём: иначе человек видит 404 и не знает, что
    // искать.
    let index = args.dist.join("index.html");
    let index_html = match tokio::fs::read_to_string(&index).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "snens-web: нет собранного клиента: {} ({e})\n\
                 собери его (`npm run build` в snens-web) или укажи --dist",
                index.display()
            );
            std::process::exit(1);
        }
    };
    let static_state = Arc::new(static_files::Static {
        dist: args.dist.clone(),
        index: index_html,
    });

    // Два роутера с разным состоянием: у прокси оно своё (адрес ядра и токен).
    // `with_state` превращает каждый в `Router<()>`, дальше их можно `merge`.
    let app = if args.proxy {
        let upstream = Arc::new(proxy::Upstream {
            base: args.api.clone(),
            token_file: args.token_file.clone(),
            client: match reqwest::Client::builder()
                // Общего таймаута нет намеренно: SSE-поток живёт часами, и
                // таймаут на весь запрос рубил бы ленту. Соединение ограничено.
                .connect_timeout(Duration::from_secs(10))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("snens-web: не создать HTTP-клиент: {e}");
                    std::process::exit(1);
                }
            },
        });
        Router::new()
            .route("/api", any(proxy::proxy))
            .route("/api/{*rest}", any(proxy::proxy))
            .with_state(upstream)
            .merge(
                Router::new()
                    .route("/", get(static_files::serve))
                    .fallback(static_files::serve)
                    .with_state(static_state),
            )
    } else {
        // Без прокси клиент берёт адрес и токен сам (пейринг, как в окне):
        // маршрута `/api` нет вовсе, и `hasProxy()` в клиенте это увидит.
        Router::new()
            .route("/", get(static_files::serve))
            .fallback(static_files::serve)
            .with_state(static_state)
    };

    let listener = match tokio::net::TcpListener::bind(args.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("snens-web: не занять {}: {e}", args.listen);
            std::process::exit(1);
        }
    };
    println!(
        "snens-web: {} · клиент {} · ядро {}",
        args.listen,
        args.dist.display(),
        if args.proxy {
            args.api.as_str()
        } else {
            "нет прокси (клиент ходит сам)"
        }
    );

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
    {
        eprintln!("snens-web: сервер упал: {e}");
        std::process::exit(1);
    }
}
