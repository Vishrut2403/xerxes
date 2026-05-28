use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::process::{Command, Stdio};

pub fn select_path(paths: &[String]) -> Result<Option<String>> {
    if which::which("fzf").is_ok() {
        return select_with_fzf(paths);
    }

    select_with_tui(paths)
}

fn select_with_fzf(paths: &[String]) -> Result<Option<String>> {
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("Failed to open /dev/tty for fzf")?;

    let tty_fd = tty.as_raw_fd();

    let saved_stdin = unsafe { libc::dup(libc::STDIN_FILENO) };
    let saved_stdout = unsafe { libc::dup(libc::STDOUT_FILENO) };

    unsafe {
        libc::dup2(tty_fd, libc::STDIN_FILENO);
        libc::dup2(tty_fd, libc::STDOUT_FILENO);
    }

    let result = (|| -> Result<Option<String>> {
        let mut child = Command::new("fzf")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("Failed to spawn fzf")?;

        {
            let stdin = child.stdin.as_mut().context("Failed to get stdin")?;
            for path in paths {
                writeln!(stdin, "{}", path)?;
            }
        }

        let output = child.wait_with_output().context("Failed to wait for fzf")?;

        if !output.status.success() {
            return Ok(None);
        }

        let res = String::from_utf8(output.stdout)
            .context("Invalid UTF-8 from fzf")?
            .trim()
            .to_string();

        Ok(if res.is_empty() { None } else { Some(res) })
    })();

    unsafe {
        libc::dup2(saved_stdin, libc::STDIN_FILENO);
        libc::dup2(saved_stdout, libc::STDOUT_FILENO);
        libc::close(saved_stdin);
        libc::close(saved_stdout);
    }

    result
}

fn select_with_tui(paths: &[String]) -> Result<Option<String>> {
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("Failed to open /dev/tty for TUI")?;

    let tty_fd = tty.as_raw_fd();
    let saved_stdin = unsafe { libc::dup(libc::STDIN_FILENO) };
    let saved_stdout = unsafe { libc::dup(libc::STDOUT_FILENO) };

    unsafe {
        libc::dup2(tty_fd, libc::STDIN_FILENO);
        libc::dup2(tty_fd, libc::STDOUT_FILENO);
    }

    let result = (|| -> Result<Option<String>> {
        enable_raw_mode()?;
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        execute!(io::stdout(), EnterAlternateScreen)?;

        let mut list_state = ListState::default();
        list_state.select(Some(0));

        let mut query = String::new();
        let mut filtered_paths: Vec<String> = paths.to_vec();
        let mut selection = None;

        loop {
            terminal.draw(|f| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .margin(1)
                    .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
                    .split(f.size());

                let input_text = format!("> {}", query);
                let input = Paragraph::new(input_text).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Filter (Type to refine) "),
                );
                f.render_widget(input, chunks[0]);

                let items: Vec<ListItem> = filtered_paths
                    .iter()
                    .map(|p| ListItem::new(p.as_str()))
                    .collect();

                let list = List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(" Matches "))
                    .highlight_style(
                        Style::default()
                            .bg(Color::Blue)
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol(">> ");

                f.render_stateful_widget(list, chunks[1], &mut list_state);
            })?;

            if let Event::Key(key) = event::read().context("Failed to read TTY event")? {
                match key.code {
                    KeyCode::Esc => break,
                    KeyCode::Enter => {
                        if let Some(i) = list_state.selected() {
                            if i < filtered_paths.len() {
                                selection = Some(filtered_paths[i].clone());
                            }
                        }
                        break;
                    }
                    KeyCode::Down => {
                        let i = match list_state.selected() {
                            Some(i) => {
                                if i >= filtered_paths.len().saturating_sub(1) {
                                    0
                                } else {
                                    i + 1
                                }
                            }
                            None => 0,
                        };
                        list_state.select(Some(i));
                    }
                    KeyCode::Up => {
                        let i = match list_state.selected() {
                            Some(i) => {
                                if i == 0 {
                                    filtered_paths.len().saturating_sub(1)
                                } else {
                                    i - 1
                                }
                            }
                            None => 0,
                        };
                        list_state.select(Some(i));
                    }
                    KeyCode::Backspace => {
                        if query.pop().is_some() {
                            filtered_paths = paths
                                .iter()
                                .filter(|p| p.to_lowercase().contains(&query.to_lowercase()))
                                .cloned()
                                .collect();
                            list_state.select(Some(0));
                        }
                    }
                    KeyCode::Char(c) => {
                        query.push(c);
                        filtered_paths = paths
                            .iter()
                            .filter(|p| p.to_lowercase().contains(&query.to_lowercase()))
                            .cloned()
                            .collect();
                        list_state.select(Some(0));
                    }
                    _ => {}
                }
            }
        }

        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen)?;
        Ok(selection)
    })();

    unsafe {
        libc::dup2(saved_stdin, libc::STDIN_FILENO);
        libc::dup2(saved_stdout, libc::STDOUT_FILENO);
        libc::close(saved_stdin);
        libc::close(saved_stdout);
    }

    result
}
