// kernava-server: MCP server binary (streamable HTTP transport, tool router, session manager)

use clap::{Parser, Subcommand};
use kernava_server as lib;

#[derive(Parser)]
#[command(
    name = "kernava",
    version,
    about = "World's fastest code intelligence MCP server"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start MCP server (streamable HTTP)
    Serve {
        #[arg(long, default_value = "8080")]
        port: u16,
        #[arg(long, default_value = "kernava.db")]
        db_path: String,
        #[arg(long, default_value = ".")]
        project_root: String,
    },
    /// Index a project from CLI (no server needed)
    Index {
        #[arg(long)]
        path: String,
        #[arg(long, default_value = "kernava.db")]
        db_path: String,
    },
    /// Print index statistics
    Stats {
        #[arg(long, default_value = "kernava.db")]
        db_path: String,
    },
    /// Run a single query tool (for debugging/scripting)
    Query {
        /// Tool name: search_symbols, get_symbol, get_callers, get_callees, etc.
        tool: String,
        /// JSON arguments for the tool (e.g. '{"query":"add"}')
        #[arg(long)]
        args: Option<String>,
        #[arg(long, default_value = "kernava.db")]
        db_path: String,
        #[arg(long, default_value = ".")]
        project_root: String,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Serve {
            port,
            db_path,
            project_root,
        } => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async { lib::serve_async(port, &db_path, &project_root).await })
        }
        Commands::Index { path, db_path } => lib::index_cmd(&path, &db_path),
        Commands::Stats { db_path } => lib::stats_cmd(&db_path),
        Commands::Query {
            tool,
            args,
            db_path,
            project_root,
        } => lib::query_cmd(&tool, &db_path, &project_root, &args),
    }
}
