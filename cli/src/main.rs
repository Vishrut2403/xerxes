use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use xerxes_core::ipc::{Request, Response};

mod tui;

#[derive(Parser)]
#[command(
    name = "xerxes",
    about = "High-performance filesystem navigation engine",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(
        trailing_var_arg = true,
        help = "The query string to search for (if no subcommand is provided)"
    )]
    query: Vec<String>,
}

#[derive(Subcommand)]
enum Commands {
    Reindex,
    Stats,
    Doctor,
    Import {
        source: String,
    },
    Init {
        shell: String,
    },
    Setup {
        #[arg(long)]
        path: Option<PathBuf>,
    },
    Alias {
        path: Option<PathBuf>,
        nickname: Option<String>,
        #[arg(long)]
        list: bool,
        #[arg(long)]
        remove: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    ClearMemory,
}

fn get_socket_path() -> Result<PathBuf> {
    let proj_dirs = ProjectDirs::from("com", "xerxes", "xerxes")
        .context("Could not find project directories")?;
    let run_dir = proj_dirs
        .runtime_dir()
        .unwrap_or_else(|| proj_dirs.data_dir());
    Ok(run_dir.join("xerxesd.sock"))
}

fn ensure_daemon_running() -> Result<()> {
    let socket_path = get_socket_path()?;
    if socket_path.exists() && UnixStream::connect(&socket_path).is_ok() {
        return Ok(());
    }

    let current_exe = std::env::current_exe()?;
    let daemon_exe = current_exe.with_file_name("_xerxesd");

    std::process::Command::new(&daemon_exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Failed to spawn _xerxesd. Is it installed in the same directory?")?;

    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if socket_path.exists() && UnixStream::connect(&socket_path).is_ok() {
            return Ok(());
        }
    }

    anyhow::bail!("Timed out waiting for _xerxesd to start.")
}

fn send_request(req: Request) -> Result<Response> {
    ensure_daemon_running()?;
    let socket_path = get_socket_path()?;
    let stream = UnixStream::connect(&socket_path)
        .context("Could not connect to _xerxesd even after trying to start it.")?;

    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;

    let mut stream = stream;

    let req_bytes = bincode::serialize(&req)?;
    stream.write_all(&req_bytes)?;
    stream.shutdown(Shutdown::Write)?;

    let mut response_bytes = Vec::new();
    stream.read_to_end(&mut response_bytes)?;

    let response: Response = bincode::deserialize(&response_bytes)?;
    Ok(response)
}

fn print_shell_init(shell: &str) {
    match shell {
        "zsh" => {
            println!(
                "{}",
                r#"
x() {
    local dir
    dir="$(_xerxes "$@")"
    if [ -n "$dir" ]; then
        cd "$dir"
    fi
}
if command -v compdef >/dev/null 2>&1; then
    compdef _cd x
fi
            "#
            );
        }
        "bash" => {
            println!(
                "{}",
                r#"
x() {
    local dir
    dir="$(_xerxes "$@")"
    if [ -n "$dir" ]; then
        cd "$dir"
    fi
}
complete -d x
            "#
            );
        }
        "fish" => {
            println!(
                "{}",
                r#"
function x
    set -l dir (_xerxes $argv)
    if test -n "$dir"
        cd "$dir"
    end
end
complete -c x -a "(__fish_complete_directories)"
            "#
            );
        }
        _ => {
            eprintln!("Unsupported shell. Please use zsh, bash, or fish.");
        }
    }
}

fn run_setup(custom_path: Option<PathBuf>) -> Result<()> {
    use directories::UserDirs;
    use std::fs::OpenOptions;
    use std::io::Write;

    let user_dirs = UserDirs::new().context("Could not determine user directories")?;
    let home = user_dirs.home_dir();

    let shell = std::env::var("SHELL").unwrap_or_default();
    let (rc_path, init_line) = if let Some(path) = custom_path {
        let expanded_path = if path.starts_with("~") {
            let without_tilde = path.to_string_lossy().to_string();
            let relative = without_tilde
                .trim_start_matches('~')
                .trim_start_matches('/');
            home.join(relative)
        } else {
            path
        };
        (
            Some(expanded_path),
            format!(
                "\neval \"$({} init zsh)\"\n",
                std::env::current_exe()?.display()
            ),
        )
    } else if shell.contains("zsh") {
        (
            Some(home.join(".zshrc")),
            format!(
                "\neval \"$({} init zsh)\"\n",
                std::env::current_exe()?.display()
            ),
        )
    } else if shell.contains("bash") {
        (
            Some(home.join(".bashrc")),
            format!(
                "\neval \"$({} init bash)\"\n",
                std::env::current_exe()?.display()
            ),
        )
    } else if shell.contains("fish") {
        (
            Some(home.join(".config/fish/config.fish")),
            format!(
                "\n{} init fish | source\n",
                std::env::current_exe()?.display()
            ),
        )
    } else {
        (None, String::new())
    };

    if let (Some(path), line) = (rc_path, init_line) {
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            if !content.contains("_xerxes init") {
                let mut file = OpenOptions::new().append(true).open(&path)?;
                file.write_all(line.as_bytes())?;
                println!("Added _xerxes init to {:?}", path);
                println!("Please restart your shell or run: source {:?}", path);
            } else {
                println!("Xerxes is already configured in {:?}", path);
            }
        } else {
            println!("Could not find shell config file at {:?}", path);
        }
    } else {
        println!("Unsupported shell or could not determine home directory.");
    }

    Ok(())
}

fn handle_query(query_parts: &[String]) -> Result<()> {
    let query_str = query_parts.join(" ");
    if query_str.is_empty() {
        return Ok(());
    }

    if query_str == ".." || query_str == "-" || query_str == "~" || query_str == "/" {
        println!("{}", query_str);
        return Ok(());
    }

    let path = std::path::Path::new(&query_str);
    if path.is_dir() {
        if let Ok(abs_path) = std::fs::canonicalize(path) {
            let p = abs_path.to_string_lossy().into_owned();
            let _ = send_request(Request::Select(p.clone()));
            println!("{}", p);
            return Ok(());
        }
    }

    let response = match send_request(Request::Query(query_str.clone())) {
        Ok(res) => res,
        Err(_) => {
            println!("Unable to find");
            return Ok(());
        }
    };

    match response {
        Response::QueryResults(paths) => {
            if paths.is_empty() {
                println!("Unable to find");
                return Ok(());
            }

            let selected = if paths.len() == 1 {
                Some(paths[0].0.clone())
            } else {
                let (top_path, top_score) = &paths[0];
                let (_, second_score) = &paths[1];

                if top_score - second_score > 50.0 {
                    Some(top_path.clone())
                } else {
                    let path_strings: Vec<String> = paths.into_iter().map(|(p, _)| p).collect();
                    tui::select_path(&path_strings)?
                }
            };

            if let Some(p) = selected {
                let _ = send_request(Request::Select(p.clone()));
                let _ = send_request(Request::RecordSelection {
                    query: query_str,
                    path: p.clone(),
                });
                println!("{}", p);
            }
        }
        Response::Error(e) => {
            eprintln!("Error from daemon: {}", e);
            std::process::exit(1);
        }
        _ => {
            eprintln!("Unexpected response from daemon.");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(cmd) = cli.command {
        match cmd {
            Commands::Reindex => {
                let response = send_request(Request::Reindex)?;
                println!("{:?}", response);
            }
            Commands::Stats => {
                let response = send_request(Request::Stats)?;
                println!("{:?}", response);
            }
            Commands::Doctor => {
                let response = send_request(Request::Doctor)?;
                println!("{:?}", response);
            }
            Commands::Import { source } => {
                println!("Import from {} not yet implemented", source);
            }
            Commands::Init { shell } => {
                print_shell_init(&shell);
            }
            Commands::Setup { path } => {
                run_setup(path)?;
            }
            Commands::Alias {
                path,
                nickname,
                list,
                remove,
                clear,
            } => {
                if list {
                    match send_request(Request::ListAliases)? {
                        Response::AliasList(aliases) => {
                            for (n, p) in aliases {
                                println!("{}: {}", n, p);
                            }
                        }
                        r => println!("{:?}", r),
                    }
                } else if let Some(n) = remove {
                    let response = send_request(Request::RemoveAlias(n))?;
                    println!("{:?}", response);
                } else if clear {
                    let response = send_request(Request::ClearAliases)?;
                    println!("{:?}", response);
                } else if let (Some(p), Some(n)) = (path, nickname) {
                    let abs_path = std::fs::canonicalize(&p).context("Could not resolve path")?;
                    let response = send_request(Request::AddAlias {
                        nickname: n,
                        path: abs_path.to_string_lossy().into_owned(),
                    })?;
                    println!("{:?}", response);
                } else {
                    eprintln!("Usage: x alias <path> <nickname> OR x alias --list OR x alias --remove <nickname> OR x alias --clear");
                }
            }
            Commands::ClearMemory => {
                let response = send_request(Request::ClearSelectionMemory)?;
                println!("{:?}", response);
            }
        }
    } else {
        handle_query(&cli.query)?;
    }

    Ok(())
}
