//! CLI stdout writes stop when the reader closes its pipe; commands still finish their effects.
//! Apply this only to output errors, never to daemon or filesystem errors.
//! Formatting macros are explicitly imported by main, directive_note, evidence::cli,
//! sekrets::cli, gate_kinds, service, and service::install; the library root never shadows
//! std's macros in daemon code or tests.

use std::fmt;
use std::io::{self, Write as _};
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

static CLOSED: AtomicBool = AtomicBool::new(false);

/// Streaming readers stop after a failed stdout write. Commands with effects keep running.
pub fn closed() -> bool {
    CLOSED.load(Ordering::Relaxed)
}

#[macro_export]
macro_rules! cli_print {
    ($($arg:tt)*) => { $crate::cli_output::print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! cli_println {
    () => { $crate::cli_print!("\n") };
    ($($arg:tt)*) => { $crate::cli_print!("{}\n", format_args!($($arg)*)) };
}

pub fn print(arguments: fmt::Arguments<'_>) {
    if closed() {
        return;
    }
    if let Err(error) = output_result(io::stdout().lock().write_fmt(arguments), ()) {
        let _ = writeln!(io::stderr().lock(), "st: write stdout: {error}");
        std::process::exit(1);
    }
}

fn output_result<T>(result: io::Result<T>, discarded: T) -> io::Result<T> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
            CLOSED.store(true, Ordering::Relaxed);
            Ok(discarded)
        }
        other => other,
    }
}

/// Include a final unterminated line in the policy; std's shutdown flush ignores errors.
pub fn finish(code: ExitCode) -> ExitCode {
    if closed() {
        return code;
    }
    match io::stdout().flush() {
        Ok(()) => code,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => code,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "st: flush stdout: {error}");
            ExitCode::FAILURE
        }
    }
}

/// A stdout writer applying the CLI policy to raw and streaming output too.
pub struct Output<W>(W);

pub fn stdout() -> Output<io::Stdout> {
    Output(io::stdout())
}

pub fn async_stdout() -> Output<tokio::io::Stdout> {
    Output(tokio::io::stdout())
}

impl Output<io::Stdout> {
    pub fn is_terminal(&self) -> bool {
        use io::IsTerminal as _;
        self.0.is_terminal()
    }
}

impl<W: io::Write> io::Write for Output<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if closed() {
            return Ok(bytes.len());
        }
        output_result(self.0.write(bytes), bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if closed() {
            return Ok(());
        }
        output_result(self.0.flush(), ())
    }
}

impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Output<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if closed() {
            return Poll::Ready(Ok(bytes.len()));
        }
        Pin::new(&mut self.0)
            .poll_write(context, bytes)
            .map(|result| output_result(result, bytes.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if closed() {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.0)
            .poll_flush(context)
            .map(|result| output_result(result, ()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if closed() {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.0)
            .poll_shutdown(context)
            .map(|result| output_result(result, ()))
    }
}
