//! `docs/glossary.nix` — the Technical Names and Technical Verbs a project declares under
//! ASD-STE100, and a strict parser for the data subset of Nix that holds them.
//!
//! Nix rather than a word-per-line list because the standard asks a Technical Name to be
//! *defined* once, and a list that cannot hold the definition pushes it into a comment. Hand-
//! written rather than `rnix` or `nix eval`: the first is a full CST parser and the second costs
//! ~200 ms against a 100 µs lint, and would not exist on a `cargo binstall` machine.
use std::{collections::HashMap, path::Path};

use color_eyre::eyre::{Result, WrapErr, bail};
use harper_brill::UPOS;

use crate::wordset::equivalent;

/// Words to their definitions. Nothing reads a definition yet; it is documentation for the human
/// and the slot `--suggest-glossary` leaves empty.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Glossary {
	pub names: HashMap<String, Option<String>>,
	pub verbs: HashMap<String, Option<String>>,
}

impl Glossary {
	pub fn read(path: &Path) -> Result<Self> {
		let src = std::fs::read_to_string(path).wrap_err_with(|| format!("failed to read glossary {}", path.display()))?;
		Self::parse(&src).wrap_err_with(|| format!("failed to parse glossary {}", path.display()))
	}

	/// A Technical Name is approved as a noun and a Technical Verb as a verb; declaring one does
	/// not declare the other. A flat list would mute a word in every part of speech, and half the
	/// words a repo wants absorbed (`list`, `pass`, `share`) have verb senses STE rejects.
	pub fn allows(&self, word: &str, used: UPOS) -> bool {
		// ADJ is not in `equivalent()` and must not be: this is not a claim about the two tagsets
		// but about position. A Technical Name in front of another noun (`chat ID`) is tagged ADJ,
		// and the standard has no separate category for an attributive noun.
		(matches!(used, UPOS::NOUN | UPOS::PROPN | UPOS::ADJ) && self.names.contains_key(word)) || (equivalent(used, UPOS::VERB) && self.verbs.contains_key(word))
	}

	pub fn parse(src: &str) -> Result<Self> {
		let mut p = Parser { src, i: 0 };
		let glossary = p.glossary()?;
		p.space();
		if p.i != src.len() {
			bail!("{}: expected end of file", p.at());
		}
		Ok(glossary)
	}
}

/// Recursive descent over the data subset only: attribute set, identifier keys, lists, double-
/// quoted strings, `#` comments. Anything else — `let`, `import`, interpolation, a function — is
/// an error rather than a partial parse.
struct Parser<'a> {
	src: &'a str,
	i: usize,
}

type Entry = (String, Option<String>);

impl Parser<'_> {
	fn glossary(&mut self) -> Result<Glossary> {
		let mut glossary = Glossary::default();
		self.expect(b'{')?;
		while !self.eat(b'}') {
			let key = self.ident()?;
			self.expect(b'=')?;
			let entries = self.list()?;
			self.expect(b';')?;
			let target = match key.as_str() {
				"names" => &mut glossary.names,
				"verbs" => &mut glossary.verbs,
				_ => {
					bail!("{}: `{key}` is not a glossary key; expected `names` or `verbs`", self.at());
				}
			};
			target.extend(entries);
		}
		Ok(glossary)
	}

	fn list(&mut self) -> Result<Vec<Entry>> {
		self.expect(b'[')?;
		let mut out = Vec::new();
		while !self.eat(b']') {
			out.push(self.entry()?);
		}
		Ok(out)
	}

	/// A bare string and `{ name = "x"; }` mean the same thing.
	fn entry(&mut self) -> Result<Entry> {
		if !self.eat(b'{') {
			return Ok((self.string()?.to_lowercase(), None));
		}
		let (mut name, mut desc) = (None, None);
		while !self.eat(b'}') {
			let key = self.ident()?;
			self.expect(b'=')?;
			let value = self.string()?;
			self.expect(b';')?;
			match key.as_str() {
				"name" => name = Some(value.to_lowercase()),
				"desc" => desc = Some(value),
				_ => {
					bail!("{}: `{key}` is not an entry field; expected `name` or `desc`", self.at());
				}
			}
		}
		let Some(name) = name else {
			bail!("{}: entry has no `name`", self.at());
		};
		Ok((name, desc))
	}

	fn string(&mut self) -> Result<String> {
		self.expect(b'"')?;
		let start = self.i;
		while let Some(c) = self.peek() {
			match c {
				b'"' => {
					let s = self.src[start..self.i].to_string();
					self.i += 1;
					return Ok(s);
				}
				b'\\' => {
					bail!("{}: escape sequences are not supported here", self.at());
				}
				b'$' if self.src.as_bytes().get(self.i + 1) == Some(&b'{') => {
					bail!("{}: string interpolation is not supported here", self.at());
				}
				_ => self.i += 1,
			}
		}
		bail!("{}: unterminated string", self.at());
	}

	fn ident(&mut self) -> Result<String> {
		self.space();
		let start = self.i;
		while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') {
			self.i += 1;
		}
		if start == self.i {
			bail!("{}: expected an attribute name", self.at());
		}
		Ok(self.src[start..self.i].to_string())
	}

	fn expect(&mut self, c: u8) -> Result<()> {
		if self.eat(c) {
			return Ok(());
		}
		bail!("{}: expected `{}`", self.at(), c as char);
	}

	fn eat(&mut self, c: u8) -> bool {
		self.space();
		let found = self.peek() == Some(c);
		self.i += usize::from(found);
		found
	}

	fn space(&mut self) {
		loop {
			match self.peek() {
				Some(c) if c.is_ascii_whitespace() => self.i += 1,
				Some(b'#') =>
					while self.peek().is_some_and(|c| c != b'\n') {
						self.i += 1;
					},
				_ => return,
			}
		}
	}

	fn peek(&self) -> Option<u8> {
		self.src.as_bytes().get(self.i).copied()
	}

	fn at(&self) -> String {
		let line = self.src[..self.i].matches('\n').count() + 1;
		let column = self.i - self.src[..self.i].rfind('\n').map_or(0, |n| n + 1) + 1;
		format!("line {line}, column {column}")
	}
}
