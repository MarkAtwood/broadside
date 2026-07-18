use anyhow::Context;
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
        /// Read markdown from stdin (mutually exclusive with positional content)
        #[arg(long, conflicts_with = "content")]
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
    /// Manage relay subscriptions
    Relay {
        #[command(subcommand)]
        command: RelayCommand,
    },
    /// One-shot poll of all configured feeds
    #[command(name = "feed-poll")]
    FeedPoll,
    /// Show overall status
    Status,
    /// Start the HTTP server
    Serve,
    /// Register with fediverse census services
    Census,
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
        /// Profile metadata field as "Name=Value" (repeatable)
        #[arg(long = "field")]
        fields: Vec<String>,
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
enum RelayCommand {
    /// Subscribe to a relay
    Add {
        /// Relay actor URL (e.g. https://relay.fedi.buzz/actor)
        url: String,
        /// Persona to send Follow from
        #[arg(long)]
        persona: String,
    },
    /// Unsubscribe from a relay
    Remove {
        /// Relay actor URL
        url: String,
        /// Persona to send Undo from
        #[arg(long)]
        persona: String,
    },
    /// List relay subscriptions
    List,
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
                broadside::db::init_data_dir(&path).await?;
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
                        fields,
                    } => {
                        let has_profile_update = display_name.is_some()
                            || bio.is_some()
                            || avatar.is_some()
                            || header.is_some();
                        if !has_profile_update && fields.is_empty() {
                            anyhow::bail!(
                                "nothing to update — specify --display-name, --bio, --avatar, --header, or --field"
                            );
                        }
                        if has_profile_update {
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
                        if !fields.is_empty() {
                            let metadata: Vec<serde_json::Value> = fields
                                .iter()
                                .filter_map(|f| {
                                    let (name, value) = f.split_once('=')?;
                                    Some(serde_json::json!({"name": name.trim(), "value": value.trim()}))
                                })
                                .collect();
                            let json = serde_json::to_string(&metadata)?;
                            sqlx::query("UPDATE personas SET metadata = ? WHERE username = ?")
                                .bind(&json)
                                .bind(&username)
                                .execute(&pool)
                                .await?;
                            println!("Set {} metadata field(s)", metadata.len());
                        }
                    }
                }
            }
            Command::Post {
                persona,
                markdown,
                content,
                media,
            } => {
                let pool = connect_db(&self.data_dir).await?;
                let data_dir = self
                    .data_dir
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("--data-dir or BROADSIDE_DATA_DIR required"))?;
                let persona_id = broadside::persona::get_id(&pool, &persona).await?;

                let text = if markdown {
                    tokio::task::spawn_blocking(|| {
                        use std::io::Read;
                        let mut buf = String::new();
                        std::io::stdin().read_to_string(&mut buf)?;
                        Ok::<_, anyhow::Error>(buf)
                    })
                    .await??
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

                for media_path in &media {
                    let path = std::path::Path::new(media_path);
                    broadside::media::process_local(&pool, &post_id, path, data_dir, "")
                        .await
                        .with_context(|| format!("processing media {media_path}"))?;
                }

                let queued = broadside::delivery::fan_out(&pool, &post_id, &persona_id).await?;
                println!(
                    "Created post {post_id} ({} media, queued {queued} deliveries)",
                    media.len()
                );
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
            Command::Relay { command } => {
                let config_path = self
                    .data_dir
                    .as_ref()
                    .map(|d| d.join("config.toml"))
                    .ok_or_else(|| anyhow::anyhow!("--data-dir or BROADSIDE_DATA_DIR required"))?;
                let config = broadside::config::Config::load(&config_path)?;
                let pool = connect_db(&self.data_dir).await?;
                match command {
                    RelayCommand::Add { url, persona } => {
                        broadside::relay::add(&pool, &url, &config.server.domain, &persona).await?;
                    }
                    RelayCommand::Remove { url, persona } => {
                        broadside::relay::remove(&pool, &url, &config.server.domain, &persona)
                            .await?;
                    }
                    RelayCommand::List => {
                        broadside::relay::list(&pool).await?;
                    }
                }
            }
            Command::Census => {
                let config_path = self
                    .data_dir
                    .as_ref()
                    .map(|d| d.join("config.toml"))
                    .ok_or_else(|| anyhow::anyhow!("--data-dir or BROADSIDE_DATA_DIR required"))?;
                let config = broadside::config::Config::load(&config_path)?;
                let domain = &config.server.domain;
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()?;

                println!("Registering {domain} with fediverse census services...");
                println!();

                let url = format!("https://the-federation.info/register/{domain}");
                match client.get(&url).send().await {
                    Ok(resp) => println!(
                        "  the-federation.info: {} {}",
                        resp.status(),
                        if resp.status().is_success() {
                            "OK"
                        } else {
                            "FAILED"
                        }
                    ),
                    Err(e) => println!("  the-federation.info: FAILED ({e})"),
                }

                let url = format!("https://fedidb.org/software/broadside");
                match client.get(&url).send().await {
                    Ok(resp) => println!(
                        "  fedidb.org: {} (crawler will pick up NodeInfo)",
                        resp.status()
                    ),
                    Err(e) => println!("  fedidb.org: FAILED ({e})"),
                }

                let url = format!("https://fediverse.observer/api/v1/instance/{domain}");
                match client.get(&url).send().await {
                    Ok(resp) => println!(
                        "  fediverse.observer: {} (crawler will discover via peers)",
                        resp.status()
                    ),
                    Err(e) => println!("  fediverse.observer: FAILED ({e})"),
                }

                println!();
                println!("Census services discover instances automatically once you federate.");
                println!("This command nudges them. Full indexing may take 24-48 hours.");
                println!();
                println!("Verify at:");
                println!("  https://the-federation.info/{domain}");
                println!("  https://fedidb.org/network?s={domain}");
                println!("  https://fediverse.observer/{domain}");
            }
            Command::FeedPoll => {
                let config_path = self
                    .data_dir
                    .as_ref()
                    .map(|d| d.join("config.toml"))
                    .ok_or_else(|| anyhow::anyhow!("--data-dir or BROADSIDE_DATA_DIR required"))?;
                let config = broadside::config::Config::load(&config_path)?;
                let pool = connect_db(&self.data_dir).await?;
                let data_dir = std::path::Path::new(&config.server.data_dir);
                broadside::feed::poll_all(&pool, &config.feed, &config.server.domain, data_dir)
                    .await?;
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
