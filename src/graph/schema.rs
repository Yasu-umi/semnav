//! Embedded schema migrations (`migrations/*.sql`), applied by refinery at
//! db-actor startup. The macro materializes a `migrations` submodule exposing
//! `runner()`.

mod embedded {
    use refinery::embed_migrations;
    embed_migrations!("migrations");
}

pub use embedded::migrations;
