use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

pub trait Prompter {
    fn say(&mut self, text: &str);
    fn choose(&mut self, question: &str, options: &[(char, &str)]) -> char;
    fn input(&mut self, question: &str, default: Option<&str>) -> String;
}

pub struct TtyPrompter {
    reader: BufReader<std::fs::File>,
    writer: std::fs::File,
}

impl TtyPrompter {
    pub fn open() -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")?;
        let reader = BufReader::new(file.try_clone()?);
        Ok(Self {
            reader,
            writer: file,
        })
    }
}

impl Prompter for TtyPrompter {
    fn say(&mut self, text: &str) {
        let _ = writeln!(self.writer, "{text}");
        let _ = self.writer.flush();
    }

    fn choose(&mut self, question: &str, options: &[(char, &str)]) -> char {
        loop {
            if !question.is_empty() {
                let _ = writeln!(self.writer, "{question}");
            }
            for (c, desc) in options {
                let _ = writeln!(self.writer, "  [{c}] {desc}");
            }
            let _ = write!(self.writer, "Choice: ");
            let _ = self.writer.flush();

            let mut line = String::new();
            if self.reader.read_line(&mut line).unwrap_or(0) == 0 {
                return options.first().map(|(c, _)| *c).unwrap_or(' ');
            }
            let trimmed = line.trim();
            if trimmed.len() == 1 {
                let ch = trimmed.chars().next().unwrap();
                if options.iter().any(|(c, _)| *c == ch) {
                    return ch;
                }
            }
        }
    }

    fn input(&mut self, question: &str, default: Option<&str>) -> String {
        if let Some(def) = default {
            let _ = write!(self.writer, "{question} [{def}]: ");
        } else {
            let _ = write!(self.writer, "{question}: ");
        }
        let _ = self.writer.flush();

        let mut line = String::new();
        let _ = self.reader.read_line(&mut line);
        let trimmed = line.trim();
        if trimmed.is_empty()
            && let Some(def) = default
        {
            return def.to_string();
        }
        trimmed.to_string()
    }
}

pub struct ScriptedPrompter {
    lines: VecDeque<String>,
}

impl ScriptedPrompter {
    pub fn new(answers: Vec<String>) -> Self {
        Self {
            lines: VecDeque::from(answers),
        }
    }

    pub fn from_file(path: impl AsRef<Path>) -> io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
        Ok(Self {
            lines: VecDeque::from(lines),
        })
    }
}

impl Prompter for ScriptedPrompter {
    fn say(&mut self, text: &str) {
        println!("{text}");
    }

    fn choose(&mut self, question: &str, options: &[(char, &str)]) -> char {
        loop {
            if !question.is_empty() {
                println!("{question}");
            }
            let line = match self.lines.pop_front() {
                Some(l) => l,
                None => panic!("{question}"),
            };
            let trimmed = line.trim();
            if trimmed.len() == 1 {
                let ch = trimmed.chars().next().unwrap();
                if options.iter().any(|(c, _)| *c == ch) {
                    return ch;
                }
            }
        }
    }

    fn input(&mut self, question: &str, default: Option<&str>) -> String {
        if !question.is_empty() {
            if let Some(def) = default {
                println!("{question} [{def}]");
            } else {
                println!("{question}");
            }
        }
        let line = match self.lines.pop_front() {
            Some(l) => l,
            None => panic!("{question}"),
        };
        let trimmed = line.trim();
        if trimmed.is_empty()
            && let Some(def) = default
        {
            return def.to_string();
        }
        trimmed.to_string()
    }
}

pub fn from_env() -> io::Result<Box<dyn Prompter>> {
    if let Ok(answers_path) = std::env::var("INSTALLCTL_TEST_ANSWERS")
        && !answers_path.is_empty()
    {
        let p = ScriptedPrompter::from_file(Path::new(&answers_path))?;
        return Ok(Box::new(p));
    }
    let p = TtyPrompter::open()?;
    Ok(Box::new(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_prompter_choose_and_reask() {
        let mut p = ScriptedPrompter::new(vec!["invalid".into(), "s".into()]);
        let c = p.choose("question", &[('s', "skip"), ('r', "run")]);
        assert_eq!(c, 's');
    }

    #[test]
    #[should_panic(expected = "choose question")]
    fn scripted_prompter_panics_on_empty_answers() {
        let mut p = ScriptedPrompter::new(vec![]);
        p.choose("choose question", &[('s', "skip")]);
    }

    #[test]
    fn scripted_prompter_input_default() {
        let mut p = ScriptedPrompter::new(vec!["".into()]);
        let val = p.input("Name", Some("default_name"));
        assert_eq!(val, "default_name");
    }
}
