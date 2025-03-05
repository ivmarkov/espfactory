/// A `std::io::Write` implementation that pulls the written data line by line
pub struct LineWrite<T> {
    /// The last line that is still not complete
    last_line: Option<String>,
    callback: T,
}

impl<T> LineWrite<T>
where
    T: FnMut(String),
{
    pub const fn new(callback: T) -> Self {
        Self {
            last_line: None,
            callback,
        }
    }

    pub fn append(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let str = core::str::from_utf8(data).unwrap_or("???");

        let mut lines = str.split('\r').peekable();

        while let Some(line) = lines.next() {
            let line = line.trim_start_matches('\n');

            let mut last_line = self.last_line.take().unwrap_or_default();
            last_line.push_str(line);

            if lines.peek().is_some() {
                (self.callback)(last_line);
                self.last_line = Some(String::new());
            } else {
                self.last_line = Some(last_line);
            }
        }
    }

    #[allow(dead_code)]
    pub fn finish(&mut self) {
        if let Some(line) = self.last_line.take() {
            (self.callback)(line);
        }
    }
}

impl<T> std::io::Write for LineWrite<T>
where
    T: FnMut(String),
{
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.append(data);

        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
