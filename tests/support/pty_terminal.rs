// Shared by integration tests (`include!("support/pty_terminal.rs")`) that
// run bazel-rs on a pseudo-terminal: the interactive progress display
// (crate::progress, docs/PROGRESS.md) only runs when stderr and stdout are a
// terminal.
//
// `on_pty(command, columns, interrupt_after)` runs `command` under a new
// pseudo-terminal of `columns` columns (Python's `pty` module: no new crate)
// and returns everything the terminal received plus the wait status; with
// `interrupt_after`, SIGINT is sent to the process once that text appeared.
// `Screen::render` replays the bytes like a terminal would (carriage return,
// line feed, cursor up, erase line and erase display; colors and titles
// ignored), so a test can assert what the user is left looking at.

/// The Python side: fork on a pty, exec the command, collect the output.
#[allow(dead_code)]
const PTY_HELPER: &str = r#"
import fcntl, os, pty, signal, struct, sys, termios
out, columns, interrupt = sys.argv[1], int(sys.argv[2]), sys.argv[3]
argv = sys.argv[4:]
pid, fd = pty.fork()
if pid == 0:
    os.execvp(argv[0], argv)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, columns, 0, 0))
data = bytearray()
sent = False
while True:
    try:
        chunk = os.read(fd, 65536)
    except OSError:
        break
    if not chunk:
        break
    data += chunk
    if interrupt and not sent and interrupt.encode() in data:
        os.kill(pid, signal.SIGINT)
        sent = True
_, status = os.waitpid(pid, 0)
open(out, "wb").write(bytes(data))
print(os.waitstatus_to_exitcode(status))
"#;

/// What the terminal received and how the process ended (its exit code, or
/// minus the signal that ended it).
#[allow(dead_code)]
struct PtyRun {
    bytes: Vec<u8>,
    status: i32,
}

/// Run `command` (program and arguments, environment and directory taken
/// from it) on a pseudo-terminal `columns` wide.
#[allow(dead_code)]
fn on_pty(command: &std::process::Command, columns: u16, interrupt_after: Option<&str>) -> PtyRun {
    let scratch = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
    let helper = scratch.path().join("pty_helper.py");
    let out = scratch.path().join("out.bin");
    std::fs::write(&helper, PTY_HELPER).unwrap();
    let mut python = std::process::Command::new("timeout");
    python
        .args(["--kill-after=5s", "600s", "python3"])
        .arg(&helper)
        .arg(&out)
        .arg(columns.to_string())
        .arg(interrupt_after.unwrap_or(""))
        .arg(command.get_program())
        .args(command.get_args());
    if let Some(directory) = command.get_current_dir() {
        python.current_dir(directory);
    }
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => python.env(name, value),
            None => python.env_remove(name),
        };
    }
    let output = python.output().unwrap();
    assert!(
        output.status.success(),
        "pty helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    PtyRun {
        bytes: std::fs::read(&out).unwrap(),
        status: String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap(),
    }
}

/// A minimal terminal: rows of characters, unbounded width.
#[allow(dead_code)]
#[derive(Default)]
struct Screen {
    rows: Vec<Vec<char>>,
    row: usize,
    column: usize,
}

#[allow(dead_code)]
impl Screen {
    /// The screen after `bytes`, as lines (trailing empty lines dropped).
    fn render(bytes: &[u8]) -> Vec<String> {
        let mut screen = Screen::default();
        let text = String::from_utf8_lossy(bytes);
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\r' => screen.column = 0,
                '\n' => screen.row += 1,
                '\x1b' => match chars.next() {
                    Some('[') => {
                        let mut parameter = String::new();
                        let mut command = ' ';
                        for c in chars.by_ref() {
                            if c.is_ascii_digit() || c == ';' || c == '?' {
                                parameter.push(c);
                            } else {
                                command = c;
                                break;
                            }
                        }
                        let count = parameter.parse::<usize>().unwrap_or(1).max(1);
                        match command {
                            'A' => screen.row = screen.row.saturating_sub(count),
                            'K' => {
                                let column = screen.column;
                                screen.line().truncate(column);
                            }
                            'J' => {
                                let column = screen.column;
                                screen.line().truncate(column);
                                screen.rows.truncate(screen.row + 1);
                            }
                            // Colors and anything else: no effect on text.
                            _ => {}
                        }
                    }
                    Some(']') => {
                        // An OSC (the title) up to its BEL.
                        for c in chars.by_ref() {
                            if c == '\x07' {
                                break;
                            }
                        }
                    }
                    _ => {}
                },
                c => {
                    let column = screen.column;
                    let line = screen.line();
                    if line.len() < column {
                        line.resize(column, ' ');
                    }
                    if line.len() == column {
                        line.push(c);
                    } else {
                        line[column] = c;
                    }
                    screen.column += 1;
                }
            }
        }
        let mut lines: Vec<String> = screen
            .rows
            .iter()
            .map(|row| row.iter().collect::<String>())
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines
    }

    fn line(&mut self) -> &mut Vec<char> {
        if self.rows.len() <= self.row {
            self.rows.resize(self.row + 1, Vec::new());
        }
        &mut self.rows[self.row]
    }
}
