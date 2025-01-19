use std::io::Write;

use crate::utils::futures::unblock;

/// The outcome of a user confirmation of a step in the task workflow
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum TaskConfirmationOutcome {
    Confirmed,
    Canceled,
    Skipped,
    Quit,
}

/// The outcome of a user input in the task workflow
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum TaskInputOutcome {
    Modified(String),
    Done(String),
    StartOver,
    Quit,
}

pub trait TaskInput {
    /// Waits for the user to:
    /// - Go back to the previous step with `Esc`
    /// - or to quit the application with `q`
    async fn wait_cancel(&mut self) -> TaskConfirmationOutcome;

    /// Waits for the user to:
    /// - Confirm the next step
    /// - or to go back to the previous step
    /// - or to quit the application
    async fn confirm(&mut self, label: &str) -> TaskConfirmationOutcome;

    /// Waits for the user to:
    /// - Confirm the next step
    /// - or to go back to the previous step
    /// - or to skip the current step
    /// - or to quit the application
    async fn confirm_or_skip(&mut self, label: &str) -> TaskConfirmationOutcome;

    async fn input(&mut self, label: &str, current: &str) -> TaskInputOutcome;

    /// Swallows all key presses
    async fn swallow(&mut self) -> !;
}

impl<T> TaskInput for &mut T
where
    T: TaskInput,
{
    async fn wait_cancel(&mut self) -> TaskConfirmationOutcome {
        TaskInput::wait_cancel(*self).await
    }

    async fn confirm(&mut self, label: &str) -> TaskConfirmationOutcome {
        TaskInput::confirm(*self, label).await
    }

    async fn confirm_or_skip(&mut self, label: &str) -> TaskConfirmationOutcome {
        TaskInput::confirm_or_skip(*self, label).await
    }

    async fn input(&mut self, label: &str, current: &str) -> TaskInputOutcome {
        TaskInput::input(*self, label, current).await
    }

    async fn swallow(&mut self) -> ! {
        TaskInput::swallow(*self).await
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum LogInputOutcome {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PgUp,
    PgDown,
    LogHome,
    LogEnd,
}

pub trait LogInput {
    async fn get(&mut self) -> LogInputOutcome;
}

impl<T> LogInput for &mut T
where
    T: LogInput,
{
    async fn get(&mut self) -> LogInputOutcome {
        LogInput::get(*self).await
    }
}

#[derive(Clone)]
pub struct Stdin;

impl Stdin {
    async fn read_line(&mut self) -> String {
        unblock("read-line", || {
            let mut line = String::new();

            std::io::stdin().read_line(&mut line)?;

            Ok(line.trim().to_string())
        })
        .await
        .unwrap()
    }
}

impl TaskInput for Stdin {
    async fn wait_cancel(&mut self) -> TaskConfirmationOutcome {
        core::future::pending().await
    }

    async fn confirm(&mut self, label: &str) -> TaskConfirmationOutcome {
        print!("{label}: ");
        std::io::stdout().flush().unwrap();

        match self.read_line().await.to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => TaskConfirmationOutcome::Confirmed,
            "c" | "cancel" | "n" | "no" => TaskConfirmationOutcome::Canceled,
            "q" | "quit" => TaskConfirmationOutcome::Quit,
            _ => unreachable!(),
        }
    }

    async fn confirm_or_skip(&mut self, label: &str) -> TaskConfirmationOutcome {
        print!("{label}: ");
        std::io::stdout().flush().unwrap();

        match self.read_line().await.to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => TaskConfirmationOutcome::Confirmed,
            "c" | "cancel" | "n" | "no" => TaskConfirmationOutcome::Canceled,
            "s" | "skip" | "i" | "ignore" => TaskConfirmationOutcome::Skipped,
            "q" | "quit" => TaskConfirmationOutcome::Quit,
            _ => unreachable!(),
        }
    }

    async fn input(&mut self, label: &str, _current: &str) -> TaskInputOutcome {
        print!("{label}: ");
        std::io::stdout().flush().unwrap();

        let line = self.read_line().await;

        if line.is_empty() {
            TaskInputOutcome::StartOver
        } else {
            TaskInputOutcome::Done(line)
        }
    }

    async fn swallow(&mut self) -> ! {
        core::future::pending().await
    }
}

impl LogInput for Stdin {
    async fn get(&mut self) -> LogInputOutcome {
        core::future::pending().await
    }
}
