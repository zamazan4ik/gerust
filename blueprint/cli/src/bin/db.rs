use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};
use {{crate_name}}_cli::util::ui::UI;
use {{crate_name}}_config::DatabaseConfig;
use {{crate_name}}_config::{load_config, parse_env, Config, Environment};
use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{
    migrate::{Migrate, Migrator},
    ConnectOptions, Connection, Executor,
};
use tokio::io::{stdin, AsyncBufReadExt};

use std::collections::HashMap;
use std::fs;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::Stdio;
use url::Url;

#[tokio::main]
async fn main() {
    cli().await;
}

#[derive(Parser)]
#[command(author, version, about = "A CLI tool to manage the project's database.", long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(short, long, global = true, help = "Choose the environment (development, test, production).", value_parser = parse_env, default_value = "development")]
    env: Environment,

    #[arg(long, global = true, help = "Disable colored output.")]
    no_color: bool,

    #[arg(long, global = true, help = "Disable debug output.")]
    quiet: bool,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Drop the database")]
    Drop,
    #[command(about = "Create the database")]
    Create,
    #[command(about = "Migrate the database")]
    Migrate,
    #[command(about = "Reset (drop, create, migrate) the database")]
    Reset,
    #[command(about = "Seed the database")]
    Seed,
    #[command(about = "Generate query metadata to support offline compile-time verification")]
    Prepare,
}

#[allow(missing_docs)]
async fn cli() {
    let cli = Cli::parse();

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let mut ui = UI::new(&mut stdout, &mut stderr, !cli.no_color, !cli.quiet);

    if let Err(e) = ensure_sqlx_cli_installed(&mut ui).await {
        ui.error("Error ensuring sqlx-cli is installed!", e);
        return;
    }

    let config: Result<Config, anyhow::Error> = load_config(&cli.env);
    match config {
        Ok(config) => match cli.command {
            Commands::Drop => {
                ui.info(&format!("Dropping {} database…", &cli.env));
                match drop(&config.database).await {
                    Ok(db_name) => {
                        ui.success(&format!("Dropped database {} successfully.", &db_name))
                    }
                    Err(e) => ui.error("Could not drop database!", e),
                }
            }
            Commands::Create => {
                ui.info(&format!("Creating {} database…", &cli.env));
                match create(&config.database).await {
                    Ok(db_name) => {
                        ui.success(&format!("Created database {} successfully.", &db_name))
                    }
                    Err(e) => ui.error("Could not create database!", e),
                }
            }
            Commands::Migrate => {
                ui.info(&format!("Migrating {} database…", &cli.env));
                ui.indent();
                match migrate(&mut ui, &config.database).await {
                    Ok(migrations) => {
                        ui.outdent();
                        ui.success(&format!("{} migrations applied.", migrations));
                    }
                    Err(e) => {
                        ui.outdent();
                        ui.error("Could not migrate database!", e);
                    }
                }
            }
            Commands::Seed => {
                ui.info(&format!("Seeding {} database…", &cli.env));
                match seed(&config.database).await {
                    Ok(_) => ui.success("Seeded database successfully."),
                    Err(e) => ui.error("Could not seed database!", e),
                }
            }
            Commands::Reset => {
                ui.info(&format!("Resetting {} database…", &cli.env));
                ui.indent();
                match reset(&mut ui, &config.database).await {
                    Ok(db_name) => {
                        ui.outdent();
                        ui.success(&format!("Reset database {} successfully.", db_name));
                    }
                    Err(e) => {
                        ui.outdent();
                        ui.error("Could not reset database!", e)
                    }
                }
            }
            Commands::Prepare => {
                let Ok(cargo) = get_cargo_path() else {
                    unreachable!("Existence of CARGO env var is asserted by calling `ensure_sqlx_cli_installed`");
                };
                let mut sqlx_prepare_command = {
                    let mut cmd = tokio::process::Command::new(&cargo);
                    cmd.args(["sqlx", "prepare"]);
                    // TODO make this path relative to gerust project root (see issue #108)
                    let cmd_cwd = {
                        let mut cwd = std::env::current_dir().unwrap();
                        cwd.push("db");
                        cwd
                    };
                    cmd.current_dir(cmd_cwd);
                    cmd.env("DATABASE_URL", &config.database.url);
                    cmd
                };

                let o = match sqlx_prepare_command.output().await {
                    Ok(o) => o,
                    Err(e) => {
                        ui.error(&format!("Could not run {cargo} sqlx prepare!"), e.into());
                        return;
                    }
                };
                if !o.status.success() {
                    ui.error(
                        &format!("Error generating query metadata. Are you sure the database is running?"),
                        anyhow!(String::from_utf8_lossy(&o.stdout).to_string()),
                    );
                    return;
                }

                ui.success("Query data written to db/.sqlx directory; please check this into version control.");
            }
        },
        Err(e) => ui.error("Could not load config!", e),
    }
}

async fn drop(config: &DatabaseConfig) -> Result<String, anyhow::Error> {
    let db_config = get_db_config(config);
    let db_name = db_config
        .get_database()
        .context("Failed to get database name!")?;
    let mut root_connection = get_root_db_client(config).await;

    let query = format!("DROP DATABASE {}", db_name);
    root_connection
        .execute(query.as_str())
        .await
        .context("Failed to drop database!")?;

    Ok(String::from(db_name))
}

async fn create(config: &DatabaseConfig) -> Result<String, anyhow::Error> {
    let db_config = get_db_config(config);
    let db_name = db_config
        .get_database()
        .context("Failed to get database name!")?;
    let mut root_connection = get_root_db_client(config).await;

    let query = format!("CREATE DATABASE {}", db_name);
    root_connection
        .execute(query.as_str())
        .await
        .context("Failed to create database!")?;

    Ok(String::from(db_name))
}

async fn migrate(ui: &mut UI<'_>, config: &DatabaseConfig) -> Result<i32, anyhow::Error> {
    let db_config = get_db_config(config);
    let migrations_path = format!("{}/../db/migrations", env!("CARGO_MANIFEST_DIR"));
    let migrator = Migrator::new(Path::new(&migrations_path))
        .await
        .context("Failed to create migrator!")?;
    let mut connection = db_config
        .connect()
        .await
        .context("Failed to connect to database!")?;

    connection
        .ensure_migrations_table()
        .await
        .context("Failed to ensure migrations table!")?;

    let applied_migrations: HashMap<_, _> = connection
        .list_applied_migrations()
        .await
        .context("Failed to list applied migrations!")?
        .into_iter()
        .map(|m| (m.version, m))
        .collect();

    let mut applied = 0;
    for migration in migrator.iter() {
        if applied_migrations.get(&migration.version).is_none() {
            connection
                .apply(migration)
                .await
                .context("Failed to apply migration {}!")?;
            ui.log(&format!("Applied migration {}.", migration.version));
            applied += 1;
        }
    }

    Ok(applied)
}

async fn seed(config: &DatabaseConfig) -> Result<(), anyhow::Error> {
    let mut connection = get_db_client(config).await;

    let statements = fs::read_to_string("./db/seeds.sql")
        .expect("Could not read seeds – make sure db/seeds.sql exists!");

    let mut transaction = connection
        .begin()
        .await
        .context("Failed to start transaction!")?;
    transaction
        .execute(statements.as_str())
        .await
        .context("Failed to execute seeds!")?;

    Ok(())
}

async fn reset(ui: &mut UI<'_>, config: &DatabaseConfig) -> Result<String, anyhow::Error> {
    ui.log("Dropping database…");
    drop(config).await?;
    ui.log("Recreating database…");
    let db_name = create(config).await?;
    ui.log("Migrating database…");
    ui.indent();
    let migration_result = migrate(ui, config).await;
    ui.outdent();

    match migration_result {
        Ok(_) => Ok(db_name),
        Err(e) => Err(e),
    }
}

fn get_db_config(config: &DatabaseConfig) -> PgConnectOptions {
    let db_url = Url::parse(&config.url).expect("Invalid DATABASE_URL!");
    ConnectOptions::from_url(&db_url).expect("Invalid DATABASE_URL!")
}

async fn get_db_client(config: &DatabaseConfig) -> PgConnection {
    let db_config = get_db_config(config);
    let connection: PgConnection = Connection::connect_with(&db_config).await.unwrap();

    connection
}

async fn get_root_db_client(config: &DatabaseConfig) -> PgConnection {
    let db_config = get_db_config(config);
    let root_db_config = db_config.clone().database("postgres");
    let connection: PgConnection = Connection::connect_with(&root_db_config).await.unwrap();

    connection
}

fn get_cargo_path() -> Result<String, anyhow::Error> {
    std::env::var("CARGO")
        .map_err(|_| anyhow!("Please invoke me using Cargo, e.g.: `cargo db <ARGS>`"))
}

/// Ensure that the correct version of sqlx-cli is installed,
/// and install it if it isn't.
async fn ensure_sqlx_cli_installed(ui: &mut UI<'_>) -> Result<(), anyhow::Error> {
    macro_rules! sqlx_cli_version {
        ($vers:literal) => {
            /// The version of sqlx-cli required
            const SQLX_CLI_VERSION: &str = $vers;
            /// The expected version output of sqlx-cli
            const SQLX_CLI_VERSION_STRING: &[u8] = concat!("sqlx-cli-sqlx ", $vers).as_bytes();
        };
    }
    sqlx_cli_version!("0.8.2");

    async fn is_sqlx_cli_installed(cargo: &str) -> Result<bool, anyhow::Error> {
        let mut cargo_sqlx_command = {
            let mut cmd = tokio::process::Command::new(cargo);
            cmd.args(["sqlx", "--version"]);
            cmd
        };

        let out = cargo_sqlx_command.output().await?;
        if out.status.success() && out.stdout.trim_ascii_end() == SQLX_CLI_VERSION_STRING {
            // sqlx-cli is installed and of the correct version
            return Ok(true);
        }

        Ok(false)
    }

    let cargo = get_cargo_path()?;

    if is_sqlx_cli_installed(&cargo).await? {
        // sqlx-cli is already installed and of the correct version, nothing to do
        return Ok(());
    }

    ui.info(
        &format!("This command requires sqlx-cli {SQLX_CLI_VERSION}, which is not installed yet. Would you like to install it now? [Y/n]"),
    );
    match {
        let mut buf = String::new();
        let mut reader = tokio::io::BufReader::new(stdin());
        // Read user answer
        loop {
            reader.read_line(&mut buf).await?;
            let line = buf.to_ascii_lowercase();
            let line = line.trim_end();
            if matches!(line, "" | "y" | "yes") {
                break ControlFlow::Continue(());
            } else if matches!(line, "n" | "no") {
                break ControlFlow::Break(());
            };
            ui.info("Please enter y or n");
            buf.clear();
        }
    } {
        ControlFlow::Continue(_) => {
            ui.info("Starting installation of sqlx-cli...");
        }
        ControlFlow::Break(_) => {
            return Err(anyhow!("Installation of sqlx-cli canceled."));
        }
    }

    let mut cargo_install_command = {
        let mut cmd = tokio::process::Command::new(&cargo);
        cmd.args([
            "install",
            "sqlx-cli",
            "--version",
            SQLX_CLI_VERSION,
            "--locked",
        ]);
        cmd.stdout(Stdio::inherit());
        cmd.stderr(Stdio::inherit());
        cmd
    };

    let mut child = cargo_install_command.spawn()?;

    let status = child.wait().await?;
    if !status.success() {
        return Err(anyhow!(
            "Something went wrong when installing sqlx-cli. Please check output"
        ));
    }

    ui.success(&format!(
        "Successfully installed sqlx-cli {SQLX_CLI_VERSION}"
    ));

    match is_sqlx_cli_installed(&cargo).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(anyhow!("sqlx-cli was not detected after installation")),
        Err(e) => Err(e),
    }
}
