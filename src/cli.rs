use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "broadside",
    about = "One-way ActivityPub server for organizations"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Path to broadside data directory
    #[arg(long, global = true, env = "BROADSIDE_DATA_DIR")]
    data_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize a new broadside data directory
    Init {
        /// Path to the data directory
        path: PathBuf,
    },
    /// Manage personas
    Persona {
        #[command(subcommand)]
        command: PersonaCommand,
    },
    /// Publish a post
    Post {
        /// Persona to post as
        #[arg(long)]
        persona: String,
        /// Read markdown from stdin
        #[arg(long)]
        markdown: bool,
        /// Attach media files
        #[arg(long)]
        media: Vec<String>,
        /// Post content (omit if using --markdown)
        content: Option<String>,
    },
    /// Manage the delivery queue
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
    },
    /// Manage followers
    Followers {
        #[command(subcommand)]
        command: FollowersCommand,
    },
    /// One-shot poll of all configured feeds
    #[command(name = "feed-poll")]
    FeedPoll,
    /// Show overall status
    Status,
    /// Start the HTTP server
    Serve,
}

#[derive(Subcommand)]
enum PersonaCommand {
    /// Create a new persona
    Add {
        /// Username for the persona
        username: String,
        /// Display name
        #[arg(long)]
        display_name: Option<String>,
    },
    /// List all personas
    List,
    /// Update a persona
    Update {
        /// Username of the persona to update
        username: String,
        /// New display name
        #[arg(long)]
        display_name: Option<String>,
        /// New bio
        #[arg(long)]
        bio: Option<String>,
        /// Path to avatar image
        #[arg(long)]
        avatar: Option<String>,
        /// Path to header image
        #[arg(long)]
        header: Option<String>,
    },
}

#[derive(Subcommand)]
enum QueueCommand {
    /// Show pending and dead-lettered deliveries
    Inspect,
    /// Retry all dead-lettered deliveries
    Retry,
    /// Show delivery statistics
    Stats,
}

#[derive(Subcommand)]
enum FollowersCommand {
    /// List followers for a persona
    List {
        /// Persona to list followers for
        #[arg(long)]
        persona: String,
    },
    /// Show follower counts per persona
    Count,
}

impl Cli {
    pub async fn run(self) -> anyhow::Result<()> {
        match self.command {
            Command::Init { path } => {
                let path_str = path.to_str().ok_or_else(|| {
                    anyhow::anyhow!("path contains invalid UTF-8: {}", path.display())
                })?;
                broadside::db::init_data_dir(path_str).await?;
                println!("Initialized broadside in {}", path.display());
            }
            Command::Persona { command } => {
                let pool = connect_db(&self.data_dir).await?;
                match command {
                    PersonaCommand::Add {
                        username,
                        display_name,
                    } => {
                        broadside::persona::add(&pool, &username, display_name.as_deref()).await?;
                    }
                    PersonaCommand::List => {
                        broadside::persona::list(&pool).await?;
                    }
                    PersonaCommand::Update {
                        username,
                        display_name,
                        bio,
                        avatar,
                        header,
                    } => {
                        broadside::persona::update(
                            &pool,
                            &username,
                            display_name.as_deref(),
                            bio.as_deref(),
                            avatar.as_deref(),
                            header.as_deref(),
                        )
                        .await?;
                    }
                }
            }
            Command::Post {
                persona,
                markdown,
                content,
                ..
            } => {
                let pool = connect_db(&self.data_dir).await?;
                let persona_id = broadside::persona::get_id(&pool, &persona).await?;

                let text = if markdown {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf
                } else {
                    content.ok_or_else(|| {
                        anyhow::anyhow!("provide content as argument or use --markdown for stdin")
                    })?
                };

                let (html, plain) = if markdown {
                    let h = broadside::sanitize::markdown_to_html(&text);
                    let t = broadside::sanitize::html_to_text(&h);
                    (h, t)
                } else {
                    let h = broadside::post::text_to_html(&text);
                    (h, text)
                };
                let post_id =
                    broadside::post::create(&pool, &persona_id, &html, &plain, None).await?;
                let queued = broadside::delivery::fan_out(&pool, &post_id, &persona_id).await?;
                println!("Created post {post_id} (queued {queued} deliveries)");
            }
            Command::Serve => {
                let config_path = self
                    .data_dir
                    .as_ref()
                    .map(|d| d.join("config.toml"))
                    .ok_or_else(|| anyhow::anyhow!("--data-dir or BROADSIDE_DATA_DIR required"))?;
                let config = broadside::config::Config::load(&config_path)?;
                broadside::server::serve(&config).await?;
            }
            Command::Status => {
                let pool = connect_db(&self.data_dir).await?;
                let (personas,) = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM personas")
                    .fetch_one(&pool)
                    .await?;
                let (followers,) = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM followers")
                    .fetch_one(&pool)
                    .await?;
                let (posts,) = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM posts")
                    .fetch_one(&pool)
                    .await?;
                let (pending,) = sqlx::query_as::<_, (i64,)>(
                    "SELECT COUNT(*) FROM delivery_queue WHERE status = 'pending'",
                )
                .fetch_one(&pool)
                .await?;
                let (dead,) = sqlx::query_as::<_, (i64,)>(
                    "SELECT COUNT(*) FROM delivery_queue WHERE status = 'dead'",
                )
                .fetch_one(&pool)
                .await?;

                println!("Personas:   {personas}");
                println!("Followers:  {followers}");
                println!("Posts:      {posts}");
                println!("Pending:    {pending}");
                println!("Dead:       {dead}");
            }
            Command::Queue { command } => {
                let pool = connect_db(&self.data_dir).await?;
                match command {
                    QueueCommand::Inspect => broadside::delivery::inspect(&pool).await?,
                    QueueCommand::Retry => broadside::delivery::retry_dead(&pool).await?,
                    QueueCommand::Stats => broadside::delivery::stats(&pool).await?,
                }
            }
            Command::Followers { command } => {
                let pool = connect_db(&self.data_dir).await?;
                match command {
                    FollowersCommand::List { persona } => {
                        let persona_id = broadside::persona::get_id(&pool, &persona).await?;
                        let rows = sqlx::query_as::<_, (String, String)>(
                            "SELECT actor_uri, followed_at FROM followers \
                             WHERE persona_id = ? ORDER BY followed_at",
                        )
                        .bind(&persona_id)
                        .fetch_all(&pool)
                        .await?;
                        if rows.is_empty() {
                            println!("No followers for @{persona}.");
                        } else {
                            for (uri, date) in &rows {
                                println!("{uri}  (since {date})");
                            }
                        }
                    }
                    FollowersCommand::Count => {
                        let rows = sqlx::query_as::<_, (String, i64)>(
                            "SELECT p.username, COUNT(f.id) \
                             FROM personas p LEFT JOIN followers f ON f.persona_id = p.id \
                             GROUP BY p.id ORDER BY p.username",
                        )
                        .fetch_all(&pool)
                        .await?;
                        for (username, count) in &rows {
                            println!("@{username}: {count}");
                        }
                    }
                }
            }
            Command::FeedPoll => {
                let config_path = self
                    .data_dir
                    .as_ref()
                    .map(|d| d.join("config.toml"))
                    .ok_or_else(|| anyhow::anyhow!("--data-dir or BROADSIDE_DATA_DIR required"))?;
                let config = broadside::config::Config::load(&config_path)?;
                let pool = connect_db(&self.data_dir).await?;
                broadside::feed::poll_all(&pool, &config.feed, &config.server.domain).await?;
            }
        }
        Ok(())
    }
}

async fn connect_db(data_dir: &Option<PathBuf>) -> anyhow::Result<sqlx::SqlitePool> {
    let dir = data_dir
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("--data-dir or BROADSIDE_DATA_DIR required"))?;
    broadside::db::connect(dir).await
}
