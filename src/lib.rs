#[cfg(test)]
mod tests;

mod app;

pub use app::app;

#[macro_use]
extern crate lazy_static;
extern crate regex;
extern crate tempdir;

use std::io::prelude::*;

use std::{fs, fmt, process, io};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Display;
use regex::Regex;

use std::os::unix::fs::PermissionsExt;

macro_rules! warn {
  ($($arg:tt)*) => {{
    extern crate std;
    use std::io::prelude::*;
    let _ = writeln!(&mut std::io::stderr(), $($arg)*);
  }};
}
macro_rules! die {
  ($($arg:tt)*) => {{
    extern crate std;
    warn!($($arg)*);
    std::process::exit(-1)
  }};
}

trait Slurp {
  fn slurp(&mut self) -> Result<String, std::io::Error>;
}

impl Slurp for fs::File {
  fn slurp(&mut self) -> Result<String, std::io::Error> {
    let mut destination = String::new();
    try!(self.read_to_string(&mut destination));
    Ok(destination)
  }
}

fn re(pattern: &str) -> Regex {
  Regex::new(pattern).unwrap()
}

#[derive(PartialEq, Debug)]
struct Recipe<'a> {
  line_number:        usize,
  name:               &'a str,
  lines:              Vec<Vec<Fragment<'a>>>,
  evaluated_lines:    Vec<String>,
  dependencies:       Vec<&'a str>,
  dependency_tokens:  Vec<Token<'a>>,
  arguments:          Vec<&'a str>,
  argument_tokens:    Vec<Token<'a>>,
  shebang:            bool,
}

#[derive(PartialEq, Debug)]
enum Fragment<'a> {
  Text{text: Token<'a>},
  Expression{expression: Expression<'a>},
}

#[derive(PartialEq, Debug)]
enum Expression<'a> {
  Variable{name: &'a str, token: Token<'a>},
  String{contents: &'a str},
  Concatination{lhs: Box<Expression<'a>>, rhs: Box<Expression<'a>>},
}

impl<'a> Expression<'a> {
  fn variables(&'a self) -> Variables<'a> {
    Variables {
      stack: vec![self],
    }
  }
}

struct Variables<'a> {
  stack: Vec<&'a Expression<'a>>,
}

impl<'a> Iterator for Variables<'a> {
  type Item = &'a Token<'a>;

  fn next(&mut self) -> Option<&'a Token<'a>> {
    match self.stack.pop() {
      None                                               => None,
      Some(&Expression::Variable{ref token,..})          => Some(token),
      Some(&Expression::String{..})                      => None,
      Some(&Expression::Concatination{ref lhs, ref rhs}) => {
        self.stack.push(lhs);
        self.stack.push(rhs);
        self.next()
      }
    }
  }
}

impl<'a> Display for Expression<'a> {
  fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
    match *self {
      Expression::Variable     {name, ..        } => try!(write!(f, "{}", name)),
      Expression::String       {contents        } => try!(write!(f, "\"{}\"", contents)),
      Expression::Concatination{ref lhs, ref rhs} => try!(write!(f, "{} + {}", lhs, rhs)),
    }
    Ok(())
  }
}

#[cfg(unix)]
fn error_from_signal(recipe: &str, exit_status: process::ExitStatus) -> RunError {
  use std::os::unix::process::ExitStatusExt;
  match exit_status.signal() {
    Some(signal) => RunError::Signal{recipe: recipe, signal: signal},
    None => RunError::UnknownFailure{recipe: recipe},
  }
}

#[cfg(windows)]
fn error_from_signal(recipe: &str, exit_status: process::ExitStatus) -> RunError {
  RunError::UnknownFailure{recipe: recipe}
}

impl<'a> Recipe<'a> {
  fn run(&self) -> Result<(), RunError<'a>> {
    if self.shebang {
      let tmp = try!(
        tempdir::TempDir::new("j")
        .map_err(|error| RunError::TmpdirIoError{recipe: self.name, io_error: error})
      );
      let mut path = tmp.path().to_path_buf();
      path.push(self.name);
      {
        let mut f = try!(
          fs::File::create(&path)
          .map_err(|error| RunError::TmpdirIoError{recipe: self.name, io_error: error})
        );
        let mut text = String::new();
        // add the shebang
        text += &self.evaluated_lines[0];
        text += "\n";
        // add blank lines so that lines in the generated script
        // have the same line number as the corresponding lines
        // in the justfile
        for _ in 1..(self.line_number + 2) {
          text += "\n"
        }
        for line in &self.evaluated_lines[1..] {
          text += &line;
          text += "\n";
        }
        try!(
          f.write_all(text.as_bytes())
          .map_err(|error| RunError::TmpdirIoError{recipe: self.name, io_error: error})
        );
      }

      // get current permissions
      let mut perms = try!(
        fs::metadata(&path)
        .map_err(|error| RunError::TmpdirIoError{recipe: self.name, io_error: error})
      ).permissions();

      // make the script executable
      let current_mode = perms.mode();
      perms.set_mode(current_mode | 0o100);
      try!(fs::set_permissions(&path, perms).map_err(|error| RunError::TmpdirIoError{recipe: self.name, io_error: error}));

      // run it!
      let status = process::Command::new(path).status();
      try!(match status {
        Ok(exit_status) => if let Some(code) = exit_status.code() {
          if code == 0 {
            Ok(())
          } else {
            Err(RunError::Code{recipe: self.name, code: code})
          }
        } else {
          Err(error_from_signal(self.name, exit_status))
        },
        Err(io_error) => Err(RunError::TmpdirIoError{recipe: self.name, io_error: io_error})
      });
    } else {
      for command in &self.evaluated_lines {
        let mut command = &command[0..];
        if !command.starts_with('@') {
          warn!("{}", command);
        } else {
          command = &command[1..]; 
        }
        let status = process::Command::new("sh")
          .arg("-cu")
          .arg(command)
          .status();
        try!(match status {
          Ok(exit_status) => if let Some(code) = exit_status.code() {
            if code == 0 {
              Ok(())
            } else {
              Err(RunError::Code{recipe: self.name, code: code})
            }
          } else {
            Err(error_from_signal(self.name, exit_status))
          },
          Err(io_error) => Err(RunError::IoError{recipe: self.name, io_error: io_error})
        });
      }
    }
    Ok(())
  }
}

impl<'a> Display for Recipe<'a> {
  fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
    try!(write!(f, "{}", self.name));
    for argument in &self.arguments {
      try!(write!(f, " {}", argument));
    }
    try!(write!(f, ":"));
    for dependency in &self.dependencies {
      try!(write!(f, " {}", dependency))
    }

    for (i, pieces) in self.lines.iter().enumerate() {
      if i == 0 {
        try!(writeln!(f, ""));
      }
      for (j, piece) in pieces.iter().enumerate() {
        if j == 0 {
          try!(write!(f, "    "));
        }
        match piece {
          &Fragment::Text{ref text} => try!(write!(f, "{}", text.lexeme)),
          &Fragment::Expression{ref expression} => try!(write!(f, "{}{}{}", "{{", expression, "}}")),
        }
      }
      if i + 1 < self.lines.len() {
        try!(write!(f, "\n"));
      }
    }
    Ok(())
  }
}

fn resolve<'a>(recipes:  &BTreeMap<&'a str, Recipe<'a>>) -> Result<(), Error<'a>> {
  let mut resolver = Resolver {
    seen:     HashSet::new(),
    stack:    vec![],
    resolved: HashSet::new(),
    recipes:  recipes,
  };
  
  for recipe in recipes.values() {
    try!(resolver.resolve(&recipe));
  }

  Ok(())
}

struct Resolver<'a: 'b, 'b> {
  stack:    Vec<&'a str>,
  seen:     HashSet<&'a str>,
  resolved: HashSet<&'a str>,
  recipes:  &'b BTreeMap<&'a str, Recipe<'a>>
}

impl<'a, 'b> Resolver<'a, 'b> {
  fn resolve(&mut self, recipe: &Recipe<'a>) -> Result<(), Error<'a>> {
    if self.resolved.contains(recipe.name) {
      return Ok(())
    }
    self.stack.push(recipe.name);
    self.seen.insert(recipe.name);
    for dependency_token in &recipe.dependency_tokens {
      match self.recipes.get(dependency_token.lexeme) {
        Some(dependency) => if !self.resolved.contains(dependency.name) {
          if self.seen.contains(dependency.name) {
            let first = self.stack[0];
            self.stack.push(first);
            return Err(dependency_token.error(ErrorKind::CircularRecipeDependency {
              recipe: recipe.name,
              circle: self.stack.iter()
                .skip_while(|name| **name != dependency.name)
                .cloned().collect()
            }));
          }
          return self.resolve(dependency);
        },
        None => return Err(dependency_token.error(ErrorKind::UnknownDependency {
          recipe:  recipe.name,
          unknown: dependency_token.lexeme
        })),
      }
    }
    self.resolved.insert(recipe.name);
    self.stack.pop();
    Ok(())
  }
}

fn evaluate<'a>(
  assignments:       &BTreeMap<&'a str, Expression<'a>>,
  assignment_tokens: &BTreeMap<&'a str, Token<'a>>,
  recipes:           &mut BTreeMap<&'a str, Recipe<'a>>,
) -> Result<BTreeMap<&'a str, String>, Error<'a>> {
  let mut evaluator = Evaluator{
    seen:              HashSet::new(),
    stack:             vec![],
    evaluated:         BTreeMap::new(),
    assignments:       assignments,
    assignment_tokens: assignment_tokens,
  };
  for name in assignments.keys() {
    try!(evaluator.evaluate_assignment(name));
  }

  for recipe in recipes.values_mut() {
    for fragments in &recipe.lines {
      let mut line = String::new();
      for fragment in fragments {
        match fragment {
          &Fragment::Text{ref text} => line += text.lexeme,
          &Fragment::Expression{ref expression} => {
            line += &try!(evaluator.evaluate_expression(&expression));
          }
        }
      }
      recipe.evaluated_lines.push(line);
    }
  }

  Ok(evaluator.evaluated)
}

struct Evaluator<'a: 'b, 'b> {
  stack:             Vec<&'a str>,
  seen:              HashSet<&'a str>,
  evaluated:         BTreeMap<&'a str, String>,
  assignments:       &'b BTreeMap<&'a str, Expression<'a>>,
  assignment_tokens: &'b BTreeMap<&'a str, Token<'a>>,
}

impl<'a, 'b> Evaluator<'a, 'b> {
  fn evaluate_assignment(&mut self, name: &'a str) -> Result<(), Error<'a>> {
    if self.evaluated.contains_key(name) {
      return Ok(());
    }

    self.stack.push(name);
    self.seen.insert(name);

    if let Some(expression) = self.assignments.get(name) {
      let value = try!(self.evaluate_expression(expression));
      self.evaluated.insert(name, value);
    } else {
      let token = self.assignment_tokens.get(name).unwrap();
      return Err(token.error(ErrorKind::UnknownVariable {variable: name}));
    }

    self.stack.pop();
    Ok(())
  }

  fn evaluate_expression(&mut self, expression: &Expression<'a>) -> Result<String, Error<'a>> {
    Ok(match *expression {
      Expression::Variable{name, ref token} => {
        if self.evaluated.contains_key(name) {
          self.evaluated.get(name).unwrap().clone()
        } else if self.seen.contains(name) {
          let token = self.assignment_tokens.get(name).unwrap();
          self.stack.push(name);
          return Err(token.error(ErrorKind::CircularVariableDependency {
            variable: name,
            circle:   self.stack.clone(),
          }));
        } else if !self.assignments.contains_key(name) {
          return Err(token.error(ErrorKind::UnknownVariable{variable: name}));
        } else {
          try!(self.evaluate_assignment(name));
          self.evaluated.get(name).unwrap().clone()
        }
      }
      Expression::String{contents} => {
        contents.to_string()
      }
      Expression::Concatination{ref lhs, ref rhs} => {
        try!(self.evaluate_expression(lhs))
        +
        &try!(self.evaluate_expression(rhs))
      }
    })
  }
}

#[derive(Debug, PartialEq)]
struct Error<'a> {
  text:   &'a str,
  index:  usize,
  line:   usize,
  column: usize,
  width:  Option<usize>,
  kind:   ErrorKind<'a>,
}

#[derive(Debug, PartialEq)]
enum ErrorKind<'a> {
  BadName{name: &'a str},
  CircularRecipeDependency{recipe: &'a str, circle: Vec<&'a str>},
  CircularVariableDependency{variable: &'a str, circle: Vec<&'a str>},
  DuplicateDependency{recipe: &'a str, dependency: &'a str},
  DuplicateArgument{recipe: &'a str, argument: &'a str},
  DuplicateRecipe{recipe: &'a str, first: usize},
  DuplicateVariable{variable: &'a str},
  ArgumentShadowsVariable{argument: &'a str},
  MixedLeadingWhitespace{whitespace: &'a str},
  ExtraLeadingWhitespace,
  InconsistentLeadingWhitespace{expected: &'a str, found: &'a str},
  OuterShebang,
  UnknownDependency{recipe: &'a str, unknown: &'a str},
  UnknownVariable{variable: &'a str},
  UnknownStartOfToken,
  UnexpectedToken{expected: Vec<TokenKind>, found: TokenKind},
  InternalError{message: String},
}

fn show_whitespace(text: &str) -> String {
  text.chars().map(|c| match c { '\t' => 't', ' ' => 's', _ => c }).collect()
}

fn mixed_whitespace(text: &str) -> bool {
  !(text.chars().all(|c| c == ' ') || text.chars().all(|c| c == '\t'))
}

struct Or<'a, T: 'a + Display>(&'a [T]);

impl<'a, T: Display> Display for Or<'a, T> {
  fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
    match self.0.len() {
      0 => {},
      1 => try!(write!(f, "{}", self.0[0])),
      2 => try!(write!(f, "{} or {}", self.0[0], self.0[1])),
      _ => for (i, item) in self.0.iter().enumerate() {
        try!(write!(f, "{}", item));
        if i == self.0.len() - 1 {
        } else if i == self.0.len() - 2 {
          try!(write!(f, ", or "));
        } else {
          try!(write!(f, ", "))
        }
      },
    }
    Ok(())
  }
}

impl<'a> Display for Error<'a> {
  fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
    try!(write!(f, "justfile:{}: ", self.line));

    match self.kind {
      ErrorKind::BadName{name} => {
         try!(writeln!(f, "name did not match /[a-z](-?[a-z0-9])*/: {}", name));
      }
      ErrorKind::CircularRecipeDependency{recipe, ref circle} => {
        if circle.len() == 2 {
          try!(write!(f, "recipe 1{} depends on itself", recipe));
        } else {
          try!(write!(f, "recipe {} has circular dependency: {}", recipe, circle.join(" -> ")));
        }
        return Ok(());
      }
      ErrorKind::CircularVariableDependency{variable, ref circle} => {
        try!(write!(f, "assignment to {} has circular dependency: {}", variable, circle.join(" -> ")));
        return Ok(());
      }
      ErrorKind::DuplicateArgument{recipe, argument} => {
        try!(writeln!(f, "recipe {} has duplicate argument: {}", recipe, argument));
      }
      ErrorKind::DuplicateVariable{variable} => {
        try!(writeln!(f, "variable \"{}\" is has multiple definitions", variable));
      }
      ErrorKind::UnexpectedToken{ref expected, found} => {
        try!(writeln!(f, "expected {} but found {}", Or(expected), found));
      }
      ErrorKind::DuplicateDependency{recipe, dependency} => {
        try!(writeln!(f, "recipe {} has duplicate dependency: {}", recipe, dependency));
      }
      ErrorKind::DuplicateRecipe{recipe, first} => {
        try!(write!(f, "duplicate recipe: {} appears on lines {} and {}", 
                    recipe, first, self.line));
        return Ok(());
      }
      ErrorKind::ArgumentShadowsVariable{argument} => {
        try!(writeln!(f, "argument {} shadows variable of the same name", argument));
      }
      ErrorKind::MixedLeadingWhitespace{whitespace} => {
        try!(writeln!(f,
          "found a mix of tabs and spaces in leading whitespace: {}\n leading whitespace may consist of tabs or spaces, but not both",
          show_whitespace(whitespace)
        ));
      }
      ErrorKind::ExtraLeadingWhitespace => {
        try!(writeln!(f, "recipe line has extra leading whitespace"));
      }
      ErrorKind::InconsistentLeadingWhitespace{expected, found} => {
        try!(writeln!(f,
          "inconsistant leading whitespace: recipe started with \"{}\" but found line with \"{}\":",
          show_whitespace(expected), show_whitespace(found)
        ));
      }
      ErrorKind::OuterShebang => {
        try!(writeln!(f, "a shebang \"#!\" is reserved syntax outside of recipes"))
      }
      ErrorKind::UnknownDependency{recipe, unknown} => {
        try!(writeln!(f, "recipe {} has unknown dependency {}", recipe, unknown));
      }
      ErrorKind::UnknownVariable{variable} => {
        try!(writeln!(f, "variable \"{}\" is unknown", variable));
      }
      ErrorKind::UnknownStartOfToken => {
        try!(writeln!(f, "unknown start of token:"));
      }
      ErrorKind::InternalError{ref message} => {
        try!(writeln!(f, "internal error, this may indicate a bug in j: {}\n consider filing an issue: https://github.com/casey/j/issues/new", message));
      }
    }

    match self.text.lines().nth(self.line) {
      Some(line) => try!(write!(f, "{}", line)),
      None => if self.index != self.text.len() {
        try!(write!(f, "internal error: Error has invalid line number: {}", self.line))
      },
    };

    Ok(())
  }
}

struct Justfile<'a> {
  recipes:     BTreeMap<&'a str, Recipe<'a>>,
  assignments: BTreeMap<&'a str, Expression<'a>>,
  values:      BTreeMap<&'a str, String>,
}

impl<'a> Justfile<'a> {
  fn first(&self) -> Option<&'a str> {
    let mut first: Option<&Recipe<'a>> = None;
    for recipe in self.recipes.values() {
      if let Some(first_recipe) = first {
        if recipe.line_number < first_recipe.line_number {
          first = Some(recipe)
        }
      } else {
        first = Some(recipe);
      }
    }
    first.map(|recipe| recipe.name)
  }

  fn count(&self) -> usize {
    self.recipes.len()
  }

  fn recipes(&self) -> Vec<&'a str> {
    self.recipes.keys().cloned().collect()
  }

  fn run_recipe(&self, recipe: &Recipe<'a>, ran: &mut HashSet<&'a str>) -> Result<(), RunError> {
    for dependency_name in &recipe.dependencies {
      if !ran.contains(dependency_name) {
        try!(self.run_recipe(&self.recipes[dependency_name], ran));
      }
    }
    try!(recipe.run());
    ran.insert(recipe.name);
    Ok(())
  }

  fn run<'b>(&'a self, names: &[&'b str]) -> Result<(), RunError<'b>> 
    where 'a: 'b
  {
    let mut missing = vec![];
    for recipe in names {
      if !self.recipes.contains_key(recipe) {
        missing.push(*recipe);
      }
    }
    if !missing.is_empty() {
      return Err(RunError::UnknownRecipes{recipes: missing});
    }
    let recipes = names.iter().map(|name| self.recipes.get(name).unwrap()).collect::<Vec<_>>();
    let mut ran = HashSet::new();
    for recipe in &recipes {
      if !recipe.arguments.is_empty() {
        return Err(RunError::MissingArguments);
      }
    }
    for recipe in recipes {
      try!(self.run_recipe(recipe, &mut ran));
    }
    Ok(())
  }

  fn get(&self, name: &str) -> Option<&Recipe<'a>> {
    self.recipes.get(name)
  }
}

impl<'a> Display for Justfile<'a> {
  fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
    let mut items = self.recipes.len() + self.assignments.len();
    for (name, expression) in &self.assignments {
      try!(write!(f, "{} = {} # \"{}\"", name, expression, self.values.get(name).unwrap()));
      items -= 1;
      if items != 0 {
        try!(write!(f, "\n"));
      }
    }
    for recipe in self.recipes.values() {
      try!(write!(f, "{}", recipe));
      items -= 1;
      if items != 0 {
        try!(write!(f, "\n"));
      }
    }
    Ok(())
  }
}

#[derive(Debug)]
enum RunError<'a> {
  UnknownRecipes{recipes: Vec<&'a str>},
  MissingArguments,
  Signal{recipe: &'a str, signal: i32},
  Code{recipe: &'a str, code: i32},
  UnknownFailure{recipe: &'a str},
  IoError{recipe: &'a str, io_error: io::Error},
  TmpdirIoError{recipe: &'a str, io_error: io::Error},
}

impl<'a> Display for RunError<'a> {
  fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
    match *self {
      RunError::UnknownRecipes{ref recipes} => {
        if recipes.len() == 1 { 
          try!(write!(f, "Justfile does not contain recipe: {}", recipes[0]));
        } else {
          try!(write!(f, "Justfile does not contain recipes: {}", recipes.join(" ")));
        };
      },
      RunError::MissingArguments => {
        try!(write!(f, "Running recipes with arguments is not yet supported"));
      },
      RunError::Code{recipe, code} => {
        try!(write!(f, "Recipe \"{}\" failed with code {}", recipe, code));
      },
      RunError::Signal{recipe, signal} => {
        try!(write!(f, "Recipe \"{}\" wast terminated by signal {}", recipe, signal));
      }
      RunError::UnknownFailure{recipe} => {
        try!(write!(f, "Recipe \"{}\" failed for an unknown reason", recipe));
      },
      RunError::IoError{recipe, ref io_error} => {
        try!(match io_error.kind() {
          io::ErrorKind::NotFound => write!(f, "Recipe \"{}\" could not be run because j could not find `sh` the command:\n{}", recipe, io_error),
          io::ErrorKind::PermissionDenied => write!(f, "Recipe \"{}\" could not be run because j could not run `sh`:\n{}", recipe, io_error),
          _ => write!(f, "Recipe \"{}\" could not be run because of an IO error while launching the `sh`:\n{}", recipe, io_error),
        });
      },
      RunError::TmpdirIoError{recipe, ref io_error} =>
        try!(write!(f, "Recipe \"{}\" could not be run because of an IO error while trying to create a temporary directory or write a file to that directory`:\n{}", recipe, io_error)),
    }
    Ok(())
  }
}

#[derive(Debug, PartialEq)]
struct Token<'a> {
  index:  usize,
  line:   usize,
  column: usize,
  text:   &'a str,
  prefix: &'a str,
  lexeme: &'a str,
  class:  TokenKind,
}

impl<'a> Token<'a> {
  fn error(&self, kind: ErrorKind<'a>) -> Error<'a> {
    Error {
      text:   self.text,
      index:  self.index + self.prefix.len(),
      line:   self.line,
      column: self.column + self.prefix.len(),
      width:  Some(self.lexeme.len()),
      kind:   kind,
    }
  }
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum TokenKind {
  Name,
  Colon,
  StringToken,
  Plus,
  Equals,
  Comment,
  Indent,
  Dedent,
  InterpolationStart,
  InterpolationEnd,
  Text,
  Line,
  Eol,
  Eof,
}

impl Display for TokenKind {
  fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
    try!(write!(f, "{}", match *self {
      Name               => "name",
      Colon              => "\":\"",
      Plus               => "\"+\"",
      Equals             => "\"=\"",
      StringToken        => "string",
      Text               => "command text",
      InterpolationStart => "{{",
      InterpolationEnd   => "}}",
      Comment            => "comment",
      Line               => "command",
      Indent             => "indent",
      Dedent             => "dedent",
      Eol                => "end of line",
      Eof                => "end of file",
    }));
    Ok(())
  }
}

use TokenKind::*;

fn token(pattern: &str) -> Regex {
  let mut s = String::new();
  s += r"^(?m)([ \t]*)(";
  s += pattern;
  s += ")";
  re(&s)
}

fn tokenize<'a>(text: &'a str) -> Result<Vec<Token>, Error> {
  lazy_static! {
    static ref EOF:                       Regex = token(r"(?-m)$"                );
    static ref NAME:                      Regex = token(r"([a-zA-Z0-9_-]+)"      );
    static ref COLON:                     Regex = token(r":"                     );
    static ref EQUALS:                    Regex = token(r"="                     );
    static ref PLUS:                      Regex = token(r"[+]"                   );
    static ref COMMENT:                   Regex = token(r"#([^!].*)?$"           );
    static ref STRING:                    Regex = token("\"[a-z0-9]\""           );
    static ref EOL:                       Regex = token(r"\n|\r\n"               );
    static ref INTERPOLATION_END:         Regex = token(r"[}][}]"                );
    static ref INTERPOLATION_START_TOKEN: Regex = token(r"[{][{]"               );
    static ref LINE:                      Regex = re(r"^(?m)[ \t]+[^ \t\n\r].*$");
    static ref INDENT:                    Regex = re(r"^([ \t]*)[^ \t\n\r]"     );
    static ref INTERPOLATION_START:       Regex = re(r"^[{][{]"                 );
    static ref LEADING_TEXT:              Regex = re(r"^(?m)(.+?)[{][{]"        );
    static ref TEXT:                      Regex = re(r"^(?m)(.+)"               );
  }

  #[derive(PartialEq)]
  enum State<'a> {
    Start,
    Indent(&'a str),
    Text,
    Interpolation,
  }

  fn indentation(text: &str) -> Option<&str> {
    INDENT.captures(text).map(|captures| captures.at(1).unwrap())
  }

  let mut tokens = vec![];
  let mut rest   = text;
  let mut index  = 0;
  let mut line   = 0;
  let mut column = 0;
  let mut state  = vec![State::Start];

  macro_rules! error {
    ($kind:expr) => {{
      Err(Error {
        text:   text,
        index:  index,
        line:   line,
        column: column,
        width:  None,
        kind:   $kind,
      })
    }};
  }

  loop {
    if column == 0 {
      if let Some(class) = match (state.last().unwrap(), indentation(rest)) {
        // ignore: was no indentation and there still isn't
        //         or current line is blank
        (&State::Start, Some("")) | (_, None) => {
          None
        }
        // indent: was no indentation, now there is
        (&State::Start, Some(current)) => {
          if mixed_whitespace(current) {
            return error!(ErrorKind::MixedLeadingWhitespace{whitespace: current})
          }
          //indent = Some(current);
          state.push(State::Indent(current));
          Some(Indent)
        }
        // dedent: there was indentation and now there isn't
        (&State::Indent(_), Some("")) => {
          // indent = None;
          state.pop();
          Some(Dedent)
        }
        // was indentation and still is, check if the new indentation matches
        (&State::Indent(previous), Some(current)) => {
          if !current.starts_with(previous) {
            return error!(ErrorKind::InconsistentLeadingWhitespace{
              expected: previous,
              found: current
            });
          }
          None
        }
        // at column 0 in some other state: this should never happen
        (&State::Text, _) | (&State::Interpolation, _) => {
          return error!(ErrorKind::InternalError{
            message: "unexpected state at column 0".to_string()
          });
        }
      } {
        tokens.push(Token {
          index:  index,
          line:   line,
          column: column,
          text:   text,
          prefix: "",
          lexeme: "",
          class:  class,
        });
      }
    }

    // insert a dedent if we're indented and we hit the end of the file
    if &State::Start != state.last().unwrap() {
      if EOF.is_match(rest) {
        tokens.push(Token {
          index:  index,
          line:   line,
          column: column,
          text:   text,
          prefix: "",
          lexeme: "",
          class:  Dedent,
        });
      }
    }

    let (prefix, lexeme, class) = 
    if let (0, &State::Indent(indent), Some(captures)) = (column, state.last().unwrap(), LINE.captures(rest)) {
      let line = captures.at(0).unwrap();
      if !line.starts_with(indent) {
        return error!(ErrorKind::InternalError{message: "unexpected indent".to_string()});
      }
      state.push(State::Text);
      (&line[0..indent.len()], "", Line)
    } else if let Some(captures) = EOF.captures(rest) {
      (captures.at(1).unwrap(), captures.at(2).unwrap(), Eof)
    } else if let &State::Text = state.last().unwrap() {
      if let Some(captures) = INTERPOLATION_START.captures(rest) {
        state.push(State::Interpolation);
        ("", captures.at(0).unwrap(), InterpolationStart)
      } else if let Some(captures) = LEADING_TEXT.captures(rest) {
        ("", captures.at(1).unwrap(), Text)
      } else if let Some(captures) = TEXT.captures(rest) {
        ("", captures.at(1).unwrap(), Text)
      } else if let Some(captures) = EOL.captures(rest) {
        state.pop();
        (captures.at(1).unwrap(), captures.at(2).unwrap(), Eol)
      } else {
        return error!(ErrorKind::InternalError{
          message: format!("Could not match token in text state: \"{}\"", rest)
        });
      }
    } else if let Some(captures) = INTERPOLATION_START_TOKEN.captures(rest) {
      (captures.at(1).unwrap(), captures.at(2).unwrap(), InterpolationStart)
    } else if let Some(captures) = INTERPOLATION_END.captures(rest) {
      if state.last().unwrap() == &State::Interpolation {
        state.pop();
      }
      (captures.at(1).unwrap(), captures.at(2).unwrap(), InterpolationEnd)
    } else if let Some(captures) = NAME.captures(rest) {
      (captures.at(1).unwrap(), captures.at(2).unwrap(), Name)
    } else if let Some(captures) = EOL.captures(rest) {
      if state.last().unwrap() == &State::Interpolation {
        panic!("interpolation must be closed at end of line");
      }
      (captures.at(1).unwrap(), captures.at(2).unwrap(), Eol)
    } else if let Some(captures) = COLON.captures(rest) {
      (captures.at(1).unwrap(), captures.at(2).unwrap(), Colon)
    } else if let Some(captures) = PLUS.captures(rest) {
      (captures.at(1).unwrap(), captures.at(2).unwrap(), Plus)
    } else if let Some(captures) = EQUALS.captures(rest) {
      (captures.at(1).unwrap(), captures.at(2).unwrap(), Equals)
    } else if let Some(captures) = COMMENT.captures(rest) {
      (captures.at(1).unwrap(), captures.at(2).unwrap(), Comment)
    } else if let Some(captures) = STRING.captures(rest) {
      (captures.at(1).unwrap(), captures.at(2).unwrap(), StringToken)
    } else if rest.starts_with("#!") {
      return error!(ErrorKind::OuterShebang)
    } else {
      return error!(ErrorKind::UnknownStartOfToken)
    };

    let len = prefix.len() + lexeme.len();

    tokens.push(Token {
      index:  index,
      line:   line,
      column: column,
      prefix: prefix,
      text:   text,
      lexeme: lexeme,
      class:  class,
    });

    if len == 0 {
      match tokens.last().unwrap().class {
        Eof => {},
        _ => return Err(tokens.last().unwrap().error(
          ErrorKind::InternalError{message: format!("zero length token: {:?}", tokens.last().unwrap())})),
      }
    }

    match tokens.last().unwrap().class {
      Eol => {
        line += 1;
        column = 0;
      },
      Eof => {
        break;
      },
      _ => {
        column += len;
      }
    }

    rest = &rest[len..];
    index += len;
  }

  Ok(tokens)
}

fn parse(text: &str) -> Result<Justfile, Error> {
  let tokens = try!(tokenize(text));
  let filtered: Vec<_> = tokens.into_iter().filter(|token| token.class != Comment).collect();
  if let Some(token) = filtered.iter().find(|token| {
    lazy_static! {
      static ref GOOD_NAME: Regex = re("^[a-z](-?[a-z0-9])*$");
    }
    token.class == Name && !GOOD_NAME.is_match(token.lexeme)
  }) {
    return Err(token.error(ErrorKind::BadName{name: token.lexeme}));
  }

  let parser = Parser{
    text: text,
    tokens: filtered.into_iter().peekable()
  };
  let justfile = try!(parser.file());
  Ok(justfile)
}

struct Parser<'a> {
  text:   &'a str,
  tokens: std::iter::Peekable<std::vec::IntoIter<Token<'a>>>
}

impl<'a> Parser<'a> {
  fn peek(&mut self, class: TokenKind) -> bool {
    self.tokens.peek().unwrap().class == class
  }

  fn accept(&mut self, class: TokenKind) -> Option<Token<'a>> {
    if self.peek(class) {
      self.tokens.next()
    } else {
      None
    }
  }

  fn accepted(&mut self, class: TokenKind) -> bool {
    self.accept(class).is_some()
  }

  fn expect(&mut self, class: TokenKind) -> Option<Token<'a>> {
    if self.peek(class) {
      self.tokens.next();
      None
    } else {
      self.tokens.next()
    }
  }

  fn expect_eol(&mut self) -> Option<Token<'a>> {
    if self.peek(Eol) {
      self.accept(Eol);
      None
    } else if self.peek(Eof) {
      None
    } else {
      self.tokens.next()
    }
  }

  fn unexpected_token(&self, found: &Token<'a>, expected: &[TokenKind]) -> Error<'a> {
    found.error(ErrorKind::UnexpectedToken {
      expected: expected.to_vec(),
      found:    found.class,
    })
  }

  fn recipe(&mut self, name: &'a str, line_number: usize) -> Result<Recipe<'a>, Error<'a>> {
    let mut arguments = vec![];
    let mut argument_tokens = vec![];
    while let Some(argument) = self.accept(Name) {
      if arguments.contains(&argument.lexeme) {
        return Err(argument.error(ErrorKind::DuplicateArgument{
          recipe: name, argument: argument.lexeme
        }));
      }
      arguments.push(argument.lexeme);
      argument_tokens.push(argument);
    }

    if let Some(token) = self.expect(Colon) {
      // if we haven't accepted any arguments, an equals
      // would have been fine as part of an assignment
      if arguments.is_empty() {
        return Err(self.unexpected_token(&token, &[Name, Colon, Equals]));
      } else {
        return Err(self.unexpected_token(&token, &[Name, Colon]));
      }
    }

    let mut dependencies = vec![];
    let mut dependency_tokens = vec![];
    while let Some(dependency) = self.accept(Name) {
      if dependencies.contains(&dependency.lexeme) {
        return Err(dependency.error(ErrorKind::DuplicateDependency {
          recipe:     name,
          dependency: dependency.lexeme
        }));
      }
      dependencies.push(dependency.lexeme);
      dependency_tokens.push(dependency);
    }

    if let Some(token) = self.expect_eol() {
      return Err(self.unexpected_token(&token, &[Name, Eol, Eof]));
    }

    let mut lines = vec![];
    let mut shebang = false;

    if self.accepted(Indent) {
      while !self.accepted(Dedent) {
        if self.accepted(Eol) {
          continue;
        }
        if let Some(token) = self.expect(Line) {
          return Err(token.error(ErrorKind::InternalError{
            message: format!("Expected a line but got {}", token.class)
          }))
        }
        let mut pieces = vec![];

        while !(self.accepted(Eol) || self.peek(Dedent)) {
          if let Some(token) = self.accept(Text) {
            if pieces.is_empty() {
              if lines.is_empty() {
                if token.lexeme.starts_with("#!") {
                  shebang = true;
                }
              } else if !shebang && token.lexeme.starts_with(" ") || token.lexeme.starts_with("\t") {
                return Err(token.error(ErrorKind::ExtraLeadingWhitespace));
              }
            }
            pieces.push(Fragment::Text{text: token});
          } else if let Some(token) = self.expect(InterpolationStart) {
            return Err(self.unexpected_token(&token, &[Text, InterpolationStart, Eol]));
          } else {
            pieces.push(Fragment::Expression{expression: try!(self.expression(true))});
            if let Some(token) = self.expect(InterpolationEnd) {
              return Err(self.unexpected_token(&token, &[InterpolationEnd]));
            }
          }
        }

        lines.push(pieces);
      }
    }

    /*
    let mut lines = vec![];
    let mut line_tokens = vec![];
    let mut shebang = false;

    if self.accepted(Indent) {
      while !self.peek(Dedent) {
        if let Some(line) = self.accept(Line) {
          if lines.is_empty() {
            if line.lexeme.starts_with("#!") {
              shebang = true;
            }
          } else if !shebang && (line.lexeme.starts_with(' ') || line.lexeme.starts_with('\t')) {
            return Err(line.error(ErrorKind::ExtraLeadingWhitespace));
          }
          lines.push(line.lexeme);
          line_tokens.push(line);
          if !self.peek(Dedent) {
            if let Some(token) = self.expect_eol() {
              return Err(self.unexpected_token(&token, &[Eol]));
            }
          }
        } else if let Some(_) = self.accept(Eol) {
        } else {
          let token = self.tokens.next().unwrap();
          return Err(self.unexpected_token(&token, &[Line, Eol]));
        }
      }

      if let Some(token) = self.expect(Dedent) {
        return Err(self.unexpected_token(&token, &[Dedent]));
      }
    }

    let mut fragments = vec![];
    let mut variables = BTreeSet::new();
    let mut variable_tokens = vec![];

    lazy_static! {
      static ref FRAGMENT:  Regex = re(r"^(.*?)\{\{(.*?)\}\}"               );
      static ref UNMATCHED: Regex = re(r"^.*?\{\{"                          );
      static ref VARIABLE:  Regex = re(r"^([ \t]*)([a-z](-?[a-z0-9])*)[ \t]*$");
    }

    for line in &line_tokens {
      let mut line_fragments = vec![];
      let mut rest = line.lexeme;
      let mut index = line.index;
      let mut column = line.column;
      while !rest.is_empty() {
        let advanced;
        if let Some(captures) = FRAGMENT.captures(rest) {
          let prefix = captures.at(1).unwrap();
          if !prefix.is_empty() {
            line_fragments.push(Fragment::Text{text: prefix});
          }
          let interior = captures.at(2).unwrap();
          if let Some(captures) = VARIABLE.captures(interior) {
            let prefix = captures.at(1).unwrap();
            let name = captures.at(2).unwrap();
            line_fragments.push(Fragment::Variable{name: name});
            variables.insert(name);
            variable_tokens.push(Token {
              index:  index + line.prefix.len(),
              line:   line.line,
              column: column + line.prefix.len(),
              text:   line.text,
              prefix: prefix,
              lexeme: name,
              class:  Name,
            });
          } else {
            return Err(line.error(ErrorKind::BadInterpolationVariableName{
              recipe: name, 
              text: interior,
            }));
          }
          advanced = captures.at(0).unwrap().len();
        } else if UNMATCHED.is_match(rest) {
          return Err(line.error(ErrorKind::UnclosedInterpolationDelimiter));
        } else {
          line_fragments.push(Fragment::Text{text: rest});
          advanced = rest.len();
        };
        index += advanced;
        column += advanced;
        rest = &rest[advanced..];
      }
      fragments.push(line_fragments);
    }
    */

    Ok(Recipe {
      line_number:       line_number,
      name:              name,
      dependencies:      dependencies,
      dependency_tokens: dependency_tokens,
      arguments:         arguments,
      argument_tokens:   argument_tokens,
      // fragments:         fragments,
      // variables:         variables,
      // variable_tokens:   variable_tokens,
      evaluated_lines:             vec![],
      lines:         lines,
      // lines:             lines,
      shebang:           shebang,
    })
  }

  fn expression(&mut self, interpolation: bool) -> Result<Expression<'a>, Error<'a>> {
    let first = self.tokens.next().unwrap();
    let lhs = match first.class {
      Name        => Expression::Variable{name: first.lexeme, token: first},
      StringToken => Expression::String{contents: &first.lexeme[1..2]},
      _           => return Err(self.unexpected_token(&first, &[Name, StringToken])),
    };

    if self.accepted(Plus) {
      let rhs = try!(self.expression(interpolation));
      Ok(Expression::Concatination{lhs: Box::new(lhs), rhs: Box::new(rhs)})
    } else if interpolation && self.peek(InterpolationEnd) {
      Ok(lhs)
    } else if let Some(token) = self.expect_eol() {
      if interpolation {
        return Err(self.unexpected_token(&token, &[Plus, Eol, InterpolationEnd]))
      } else {
        Err(self.unexpected_token(&token, &[Plus, Eol]))
      }
    } else {
      Ok(lhs)
    }
  }

  fn file(mut self) -> Result<Justfile<'a>, Error<'a>> {
    let mut recipes = BTreeMap::<&str, Recipe>::new();
    let mut assignments = BTreeMap::<&str, Expression>::new();
    let mut assignment_tokens = BTreeMap::<&str, Token<'a>>::new();

    loop {
      match self.tokens.next() {
        Some(token) => match token.class {
          Eof => break,
          Eol => continue,
          Name => if self.accepted(Equals) {
            if assignments.contains_key(token.lexeme) {
              return Err(token.error(ErrorKind::DuplicateVariable {
                variable: token.lexeme,
              }));
            }
            assignments.insert(token.lexeme, try!(self.expression(false)));
            assignment_tokens.insert(token.lexeme, token);
          } else {
            if let Some(recipe) = recipes.remove(token.lexeme) {
              return Err(token.error(ErrorKind::DuplicateRecipe {
                recipe: recipe.name,
                first:  recipe.line_number
              }));
            }
            recipes.insert(token.lexeme, try!(self.recipe(token.lexeme, token.line)));
          },
          Comment => return Err(token.error(ErrorKind::InternalError {
            message: "found comment in token stream".to_string()
          })),
          _ => return return Err(self.unexpected_token(&token, &[Name])),
        },
        None => return Err(Error {
          text:   self.text,
          index:  0,
          line:   0,
          column: 0,
          width:  None,
          kind:   ErrorKind::InternalError {
            message: "unexpected end of token stream".to_string()
          }
        }),
      }
    }

    if let Some(token) = self.tokens.next() {
      return Err(token.error(ErrorKind::InternalError{
        message: format!("unexpected token remaining after parsing completed: {:?}", token.class)
      }))
    }

    try!(resolve(&recipes));

    for recipe in recipes.values() {
      for argument in &recipe.argument_tokens {
        if assignments.contains_key(argument.lexeme) {
          return Err(argument.error(ErrorKind::ArgumentShadowsVariable {
            argument: argument.lexeme
          }));
        }
      }

      for line in &recipe.lines {
        for piece in line {
          if let &Fragment::Expression{ref expression} = piece {
            for variable in expression.variables() {
              let name = variable.lexeme;
              if !(assignments.contains_key(&name) || recipe.arguments.contains(&name)) {
                // There's a borrow issue here that seems to difficult to solve.
                // The error derived from the variable token has too short a lifetime,
                // so we create a new error from its contents, which do live long
                // enough.
                //
                // I suspect the solution here is to give recipes, pieces, and expressions
                // two lifetime parameters instead of one, with one being the lifetime
                // of the struct, and the second being the lifetime of the tokens
                // that it contains
                let error = variable.error(ErrorKind::UnknownVariable{variable: name});
                return Err(Error {
                  text:   self.text,
                  index:  error.index,
                  line:   error.line,
                  column: error.column,
                  width:  error.width,
                  kind:   ErrorKind::UnknownVariable {
                    variable: &self.text[error.index..error.index + error.width.unwrap()],
                  }
                });
              }
            }
          }
        }
      }
    }

    let values = try!(evaluate(&assignments, &assignment_tokens, &mut recipes));

    Ok(Justfile{
      recipes:     recipes,
      assignments: assignments,
      values:      values,
    })
  }
}
