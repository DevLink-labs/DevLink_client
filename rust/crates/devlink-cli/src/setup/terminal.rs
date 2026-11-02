//! Terminal adapter: CLI flags and rendering, separate from setup orchestration.
use super::{Completion, ProgressEvent, ProgressUnavailable, RunResult, SetupContext, SetupStages};
use clap::{Args, ValueEnum};
use serde::Serialize;
use std::{fmt, io::Write, path::Path};

#[derive(Args)]
#[command(
    after_help = "Setup initializes missing configuration and validates existing settings.\nInput collection and remaining stage integrations will be added in subsequent work.\nExisting auth, team, agent, start, and mcp commands remain available."
)]
pub struct SetupArgs {
    /// Output format: final JSON only, JSONL progress, or human-readable messages
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub output: OutputFormat,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Human,
    Json,
    Jsonl,
}

/// Carries an already-rendered setup exit code without printing a second error.
#[derive(Debug)]
pub struct SetupExit(pub u8);

impl fmt::Display for SetupExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Setup did not complete")
    }
}

impl std::error::Error for SetupExit {}

pub async fn run(path: &Path, args: SetupArgs) -> anyhow::Result<()> {
    let context = SetupContext {
        config_path: path.to_path_buf(),
    };
    let stdout = std::io::stdout();
    let mut output = Renderer::new(stdout.lock(), args.output);
    let result = super::run(&context, SetupStages::default(), |event| {
        output.progress(event)
    })
    .await;
    output.result(&result).map_err(|_| SetupExit(1))?;
    if result.exit_code != 0 {
        return Err(SetupExit(result.exit_code).into());
    }
    Ok(())
}

/// Caller-supplied writer also supports tests and non-terminal embedding.
pub struct Renderer<W> {
    writer: W,
    format: OutputFormat,
}

impl<W: Write> Renderer<W> {
    pub fn new(writer: W, format: OutputFormat) -> Self {
        Self { writer, format }
    }

    pub fn progress(&mut self, event: &ProgressEvent) -> Result<(), ProgressUnavailable> {
        match self.format {
            OutputFormat::Json => Ok(()),
            OutputFormat::Jsonl => self.json(event),
            OutputFormat::Human => {
                let step = serde_json::to_value(event.step).map_err(|_| ProgressUnavailable)?;
                let status =
                    serde_json::to_value(event.step_status).map_err(|_| ProgressUnavailable)?;
                writeln!(
                    self.writer,
                    "{}: {}",
                    step.as_str().unwrap_or("setup"),
                    status.as_str().unwrap_or("unknown")
                )
                .and_then(|_| self.writer.flush())
                .map_err(|_| ProgressUnavailable)
            }
        }
    }

    pub fn result<D: Serialize>(
        &mut self,
        result: &RunResult<D>,
    ) -> Result<(), ProgressUnavailable> {
        match self.format {
            OutputFormat::Json | OutputFormat::Jsonl => self.json(result),
            OutputFormat::Human => {
                let message = match &result.completion {
                    Completion::Success { .. } => "Setup completed.",
                    Completion::Failure { error } => error.message(),
                };
                writeln!(self.writer, "{message}").map_err(|_| ProgressUnavailable)?;
                if let Completion::Failure { error } = &result.completion {
                    write!(self.writer, "Stage: ").map_err(|_| ProgressUnavailable)?;
                    self.json(&error.step)?;
                    if let Some(location) = &error.location {
                        write!(self.writer, "Location: ").map_err(|_| ProgressUnavailable)?;
                        self.json(location)?;
                    }
                    writeln!(self.writer, "Recovery: {}", error.recovery_action())
                        .map_err(|_| ProgressUnavailable)?;
                }
                self.writer.flush().map_err(|_| ProgressUnavailable)
            }
        }
    }

    fn json(&mut self, value: &impl Serialize) -> Result<(), ProgressUnavailable> {
        let json = serde_json::to_string(value).map_err(|_| ProgressUnavailable)?;
        // Match the CLI's existing escaping of control/bidi characters in public values.
        for c in json.chars() {
            if matches!(c, '\u{7f}'..='\u{9f}' | '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
            {
                write!(self.writer, "\\u{:04x}", c as u32)
            } else {
                write!(self.writer, "{c}")
            }
            .map_err(|_| ProgressUnavailable)?;
        }
        writeln!(self.writer)
            .and_then(|_| self.writer.flush())
            .map_err(|_| ProgressUnavailable)
    }
}
