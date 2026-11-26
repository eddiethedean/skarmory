//! Bounded, lexical interpretation of selected Django template lookups.

use super::*;

const MAX_TOKENS: usize = 8_192;
const MAX_SCOPES: usize = 64;

#[derive(Clone)]
enum Token {
    Output(String),
    Tag(Vec<String>),
}

impl Token {
    fn tag(&self) -> Option<(&str, &[String])> {
        let Self::Tag(words) = self else { return None };
        let (name, arguments) = words.split_first()?;
        Some((name, arguments))
    }
}

const UNSUPPORTED: &str = "template syntax is outside the bounded Django context model";

pub(super) fn references(
    project: &TemplateProject<'_>,
    directories: &[PathBuf],
    name: &str,
    aliases: BTreeSet<String>,
) -> Result<BTreeSet<String>, Issue> {
    let mut render = Render {
        project,
        directories,
        methods: BTreeSet::new(),
        fuel: MAX_TOKENS,
    };
    render.template(name, aliases, 0)?;
    Ok(render.methods)
}

struct Render<'a> {
    project: &'a TemplateProject<'a>,
    directories: &'a [PathBuf],
    methods: BTreeSet<String>,
    fuel: usize,
}

enum Scope {
    With,
    For,
    If {
        branches: Vec<BTreeSet<String>>,
        has_else: bool,
    },
}

impl Render<'_> {
    fn template(
        &mut self,
        name: &str,
        aliases: BTreeSet<String>,
        depth: usize,
    ) -> Result<bool, Issue> {
        let Some(tokens) = self.program(name, depth)? else {
            return Ok(false);
        };
        self.walk(name, tokens, aliases, depth)?;
        Ok(true)
    }

    fn program(&mut self, name: &str, depth: usize) -> Result<Option<Vec<Token>>, Issue> {
        if depth >= MAX_TEMPLATE_DEPTH {
            return Err(Issue::new(
                "analysis_limit",
                name,
                "Django template nesting limit exceeded",
            ));
        }
        let Some(source) = self.project.load(self.directories, name)? else {
            return Ok(None);
        };
        let tokens = tokenize(&source).ok_or_else(|| unsupported(name))?;
        self.fuel = self.fuel.checked_sub(tokens.len()).ok_or_else(|| {
            Issue::new(
                "analysis_limit",
                name,
                "Django template token limit exceeded",
            )
        })?;
        let mut parents = tokens
            .iter()
            .filter_map(Token::tag)
            .filter(|(tag, _)| *tag == "extends");
        let Some((_, parent)) = parents.next() else {
            return Ok(Some(tokens));
        };
        let [parent] = parent else {
            return Err(unsupported(name));
        };
        let Some(parent) = quoted(parent) else {
            return Err(unsupported(name));
        };
        if parents.next().is_some()
            || tokens.first().and_then(Token::tag).map(|(tag, _)| tag) != Some("extends")
        {
            return Err(unsupported(name));
        }
        let overrides = blocks(&tokens).ok_or_else(|| unsupported(name))?;
        let Some(mut inherited) = self.program(parent, depth + 1)? else {
            return Err(unresolved_dependency(name, parent));
        };
        // Replace outer blocks first. The block boundaries remain available
        // to descendants and so nested replaced blocks are skipped.
        let mut index = 0;
        while index < inherited.len() {
            if let Some(("block", [block])) = inherited[index].tag()
                && let Some(replacement) = overrides.get(block.as_str())
            {
                let end = block_end(&inherited, index).ok_or_else(|| unsupported(name))?;
                if inherited.len() - (end - index - 1) + replacement.len() > MAX_TOKENS {
                    return Err(Issue::new(
                        "analysis_limit",
                        name,
                        "Django template block expansion limit exceeded",
                    ));
                }
                inherited.splice(index + 1..end, replacement.iter().cloned());
                index += replacement.len() + 1;
            }
            index += 1;
        }
        Ok(Some(inherited))
    }

    fn walk(
        &mut self,
        name: &str,
        tokens: Vec<Token>,
        mut aliases: BTreeSet<String>,
        depth: usize,
    ) -> Result<(), Issue> {
        let mut scopes = Vec::<(Scope, BTreeSet<String>)>::new();
        let mut blocks = Vec::new();
        for token in &tokens {
            if let Token::Output(expression) = token {
                self.operand(name, expression, &aliases)?;
                continue;
            }
            let Some((tag, args)) = token.tag() else {
                return Err(unsupported(name));
            };
            match (tag, args) {
                ("with", arguments) => {
                    let bindings = bindings(arguments).ok_or_else(|| unsupported(name))?;
                    let next = self.bindings(name, &aliases, bindings)?;
                    scopes.push((Scope::With, std::mem::replace(&mut aliases, next)));
                }
                ("endwith", []) => {
                    let Some((Scope::With, parent)) = scopes.pop() else {
                        return Err(unsupported(name));
                    };
                    aliases = parent;
                }
                ("for", arguments) => {
                    let split = arguments
                        .iter()
                        .position(|word| word == "in")
                        .ok_or_else(|| unsupported(name))?;
                    let (names, iterable) = arguments.split_at(split);
                    let iter = match &iterable[1..] {
                        [iter] => iter,
                        [iter, reversed] if reversed == "reversed" => iter,
                        _ => return Err(unsupported(name)),
                    };
                    self.operand(name, iter, &aliases)?;
                    let names = names.join(" ");
                    if names.is_empty() {
                        return Err(unsupported(name));
                    }
                    let mut next = aliases.clone();
                    for variable in names.split(',').map(str::trim) {
                        if !skarmory_python::simple_name(variable) {
                            return Err(unsupported(name));
                        }
                        next.remove(variable);
                    }
                    scopes.push((Scope::For, std::mem::replace(&mut aliases, next)));
                }
                ("empty", []) => {
                    let Some((Scope::For, parent)) = scopes.last() else {
                        return Err(unsupported(name));
                    };
                    aliases.clone_from(parent);
                }
                ("endfor", []) => {
                    let Some((Scope::For, parent)) = scopes.pop() else {
                        return Err(unsupported(name));
                    };
                    aliases = parent;
                }
                ("if", conditions) if !conditions.is_empty() => {
                    self.operands(name, conditions, &aliases)?;
                    scopes.push((
                        Scope::If {
                            branches: Vec::new(),
                            has_else: false,
                        },
                        aliases.clone(),
                    ));
                }
                ("elif" | "else", conditions) => {
                    let Some((Scope::If { branches, has_else }, parent)) = scopes.last_mut() else {
                        return Err(unsupported(name));
                    };
                    if *has_else || (tag == "else") != conditions.is_empty() {
                        return Err(unsupported(name));
                    }
                    *has_else = tag == "else";
                    branches.push(std::mem::replace(&mut aliases, parent.clone()));
                    self.operands(name, conditions, &aliases)?;
                }
                ("endif", []) => {
                    let Some((Scope::If { branches, has_else }, parent)) = scopes.pop() else {
                        return Err(unsupported(name));
                    };
                    aliases.extend(branches.into_iter().flatten());
                    if !has_else {
                        aliases.extend(parent);
                    }
                }
                ("firstof", [items @ .., keyword, target]) if keyword == "as" => {
                    self.operands(name, items, &aliases)?;
                    if items.iter().any(|item| aliases.contains(item)) {
                        aliases.insert(target.clone());
                    } else {
                        aliases.remove(target);
                    }
                }
                ("firstof", items) => self.operands(name, items, &aliases)?,
                ("include", [child, tail @ ..]) => {
                    let child = quoted(child).ok_or_else(|| unsupported(name))?;
                    let (only, args) = match tail.split_last() {
                        Some((last, rest)) if last == "only" => (true, rest),
                        _ => (false, tail),
                    };
                    let base = if only {
                        BTreeSet::new()
                    } else {
                        aliases.clone()
                    };
                    let next = match args {
                        [] => base,
                        [keyword, arguments @ ..] if keyword == "with" => {
                            let bindings = bindings(arguments).ok_or_else(|| unsupported(name))?;
                            let values = self.bindings(name, &aliases, bindings.clone())?;
                            let mut next = base;
                            for (key, _) in bindings {
                                if values.contains(key) {
                                    next.insert(key.to_owned());
                                } else {
                                    next.remove(key);
                                }
                            }
                            next
                        }
                        _ => return Err(unsupported(name)),
                    };
                    if !self.template(child, next, depth + 1)? {
                        return Err(unresolved_dependency(name, child));
                    }
                }
                ("block", [block]) => blocks.push(block),
                ("endblock", label) => {
                    let Some(block) = blocks.pop() else {
                        return Err(unsupported(name));
                    };
                    if !label.is_empty() && label != [block.clone()] {
                        return Err(unsupported(name));
                    }
                }
                ("csrf_token", []) => {}
                _ => return Err(unsupported(name)),
            }
            if scopes.len() > MAX_SCOPES || aliases.len() > MAX_SCOPES {
                return Err(Issue::new(
                    "analysis_limit",
                    name,
                    "Django template context limit exceeded",
                ));
            }
        }
        if scopes.is_empty() && blocks.is_empty() {
            Ok(())
        } else {
            Err(unsupported(name))
        }
    }

    fn bindings(
        &mut self,
        name: &str,
        aliases: &BTreeSet<String>,
        bindings: Vec<(&str, &str)>,
    ) -> Result<BTreeSet<String>, Issue> {
        let mut next = aliases.clone();
        for (target, value) in bindings {
            if !skarmory_python::simple_name(target) {
                return Err(unsupported(name));
            }
            self.operand(name, value, aliases)?;
            if aliases.contains(value) {
                next.insert(target.to_owned());
            } else {
                next.remove(target);
            }
        }
        Ok(next)
    }

    fn operands(
        &mut self,
        name: &str,
        words: &[String],
        aliases: &BTreeSet<String>,
    ) -> Result<(), Issue> {
        for word in words {
            self.operand(name, word, aliases)?;
        }
        Ok(())
    }

    fn operand(
        &mut self,
        name: &str,
        expression: &str,
        aliases: &BTreeSet<String>,
    ) -> Result<(), Issue> {
        let expression = expression.trim();
        if expression == "block.super" || expression.contains('|') && quoted(expression).is_none() {
            return Err(unsupported(name));
        }
        if quoted(expression).is_none()
            && let Some((alias, members)) = expression.split_once('.')
            && aliases.contains(alias)
            && let Some(member) = members
                .split('.')
                .next()
                .filter(|part| skarmory_python::simple_name(part))
            && !member.starts_with('_')
        {
            self.methods.insert(member.to_owned());
        }
        Ok(())
    }
}

fn unsupported(name: &str) -> Issue {
    Issue::new("unresolved_django_template", name, UNSUPPORTED)
}

fn unresolved_dependency(name: &str, dependency: &str) -> Issue {
    Issue::new(
        "unresolved_django_template",
        name,
        format!("could not select Django template {dependency:?}"),
    )
}

fn bindings(words: &[String]) -> Option<Vec<(&str, &str)>> {
    if let [value, keyword, alias] = words
        && keyword == "as"
    {
        return Some(vec![(alias, value)]);
    }
    (!words.is_empty()).then_some(())?;
    words.iter().map(|word| word.split_once('=')).collect()
}

fn blocks(tokens: &[Token]) -> Option<BTreeMap<&str, &[Token]>> {
    let mut result = BTreeMap::new();
    for (index, token) in tokens.iter().enumerate() {
        if let Some(("block", [name])) = token.tag() {
            let end = block_end(tokens, index)?;
            if result
                .insert(name.as_str(), &tokens[index + 1..end])
                .is_some()
            {
                return None;
            }
        }
    }
    Some(result)
}

fn block_end(tokens: &[Token], start: usize) -> Option<usize> {
    let mut depth = 0;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        let Some((tag, _)) = token.tag() else {
            continue;
        };
        match tag {
            "block" => depth += 1,
            "endblock" => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn quoted(value: &str) -> Option<&str> {
    let quote = value.chars().next()?;
    if !matches!(quote, '\'' | '"') {
        return None;
    }
    value.strip_prefix(quote)?.strip_suffix(quote)
}

fn words(source: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in source.char_indices() {
        if escaped {
            escaped = false;
        } else if let Some(expected) = quote {
            match ch {
                '\\' => escaped = true,
                ch if ch == expected => quote = None,
                _ => {}
            }
        } else if matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if ch.is_whitespace() {
            if index > start {
                words.push(source[start..index].to_owned());
            }
            start = index + ch.len_utf8();
        }
    }
    if quote.is_some() {
        return None;
    }
    if start < source.len() {
        words.push(source[start..].to_owned());
    }
    Some(words)
}

fn tokenize(mut source: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    while let Some(open) = source.find('{') {
        source = &source[open + 1..];
        let (kind, delimiter) = match source.as_bytes().first() {
            Some(b'{') => (b'{', "}}"),
            Some(b'%') => (b'%', "%}"),
            Some(b'#') => (b'#', "#}"),
            _ => continue,
        };
        let contents = &source[1..];
        let Some((contents, rest)) = contents.split_once(delimiter) else {
            continue;
        };
        if contents.contains('\n') {
            continue;
        }
        source = rest;
        let contents = contents.trim();
        match kind {
            b'#' => {}
            b'{' => tokens.push(Token::Output(contents.to_owned())),
            _ => {
                let words = words(contents)?;
                match words.first().map(String::as_str) {
                    Some("comment") => source = end_raw_tag(source, "endcomment")?,
                    Some("verbatim") if words.len() == 1 => {
                        source = end_raw_tag(source, "endverbatim")?
                    }
                    _ => tokens.push(Token::Tag(words)),
                }
            }
        }
        if tokens.len() > MAX_TOKENS {
            return None;
        }
    }
    Some(tokens)
}

fn end_raw_tag<'a>(mut source: &'a str, name: &str) -> Option<&'a str> {
    while let Some((_, tag)) = source.split_once("{%") {
        let (contents, rest) = tag.split_once("%}")?;
        if contents.trim() == name {
            return Some(rest);
        }
        source = rest;
    }
    None
}
