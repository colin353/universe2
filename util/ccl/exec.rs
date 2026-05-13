use crate::ast;
use crate::eval;
use crate::{Dictionary, ImportResolver, Value};

use ggen::{GrammarUnit, ParseError};

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
enum ImportedIdentifier {
    Direct(String),
    Aliased(String),
}

impl ImportedIdentifier {
    fn as_str(&self) -> &str {
        match self {
            Self::Direct(c) | Self::Aliased(c) => c.as_str(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ExecError {
    CannotResolveSymbol(ParseError),
    OperatorWithInvalidType(ParseError),
    ArraysCannotContainDictionaries(ParseError),
    ImportResolutionError(String),
    ImportParsingError(ParseError),
}

impl ExecError {
    pub fn render(&self, content: &str) -> String {
        match self {
            Self::CannotResolveSymbol(e)
            | Self::ImportParsingError(e)
            | Self::OperatorWithInvalidType(e)
            | Self::ArraysCannotContainDictionaries(e) => e.render(content),
            Self::ImportResolutionError(e) => e.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ValueOrScope<'a> {
    Value(Value),
    Scope(Scope<'a>),
}

#[derive(Debug, Clone)]
pub struct Scope<'a> {
    pub inner: Arc<Mutex<ScopeInner<'a>>>,
}

#[derive(Debug, Clone)]
pub struct ScopeInner<'a> {
    in_progress_identifiers: HashSet<String>,
    resolved_identifiers: HashMap<String, ValueOrScope<'a>>,
    unresolved_identifiers: HashMap<String, ast::Expression>,
    unresolved_imports: HashMap<String, ImportedIdentifier>,
    scopes: HashMap<String, Scope<'a>>,
    default_value: Option<ast::Expression>,
    content: Cow<'a, str>,
    parent_scope: Option<Scope<'a>>,
    overrides: Vec<Scope<'a>>,
    pub deep_overrides: HashMap<String, HashMap<String, (Scope<'a>, ast::Expression)>>,
    start: usize,
    end: usize,
    import_resolvers: Vec<Arc<dyn ImportResolver>>,
    import_context: Option<String>,
}

impl<'a> Scope<'a> {
    pub fn empty(content: Cow<'a, str>, start: usize, end: usize) -> Self {
        let inner = ScopeInner {
            in_progress_identifiers: HashSet::new(),
            resolved_identifiers: HashMap::new(),
            unresolved_identifiers: HashMap::new(),
            unresolved_imports: HashMap::new(),
            scopes: HashMap::new(),
            parent_scope: None,
            overrides: Vec::new(),
            default_value: None,
            content: content,
            deep_overrides: HashMap::new(),
            start,
            end,
            import_resolvers: Vec::new(),
            import_context: None,
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    pub fn scope_content(&self) -> String {
        let _inner = self.inner.lock().unwrap();
        _inner.content[_inner.start.._inner.end].to_string()
    }

    pub fn duplicate(&self) -> Self {
        let inner = self.inner.try_lock().unwrap().clone();
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    pub fn add_override(&self, override_scope: Scope<'a>) {
        self.inner
            .try_lock()
            .unwrap()
            .overrides
            .push(override_scope);
    }

    pub fn add_deep_overrides(
        &self,
        name: String,
        overrides: &HashMap<String, (Scope<'a>, ast::Expression)>,
    ) {
        let mut inner = self.inner.try_lock().unwrap();
        let entry = inner
            .deep_overrides
            .entry(name)
            .or_insert_with(HashMap::new);
        for o in overrides {
            entry.insert(o.0.to_string(), o.1.clone());
        }
    }

    pub fn from_module(module: ast::Module, content: Cow<'a, str>) -> Self {
        let (start, end) = module.range();
        let out = Self::empty(content.clone(), start, end);

        for import in module.imports {
            let mut _inner = out.inner.try_lock().unwrap();
            match import.spec {
                ast::ImportSpecification::Multiple(multi) => {
                    for ident in multi.identifiers.values {
                        _inner.unresolved_imports.insert(
                            ident.as_str(content.as_ref()).to_string(),
                            ImportedIdentifier::Direct(import.from.value.clone()),
                        );
                    }
                }
                ast::ImportSpecification::Single(ident) => {
                    _inner.unresolved_imports.insert(
                        ident.as_str(content.as_ref()).to_string(),
                        ImportedIdentifier::Aliased(import.from.value.clone()),
                    );
                }
            }
        }

        for b in module.bindings {
            let lvalue = b.assignment.left;
            if lvalue.values.len() > 1 {
                let override_target = lvalue.values[0].as_str(content.as_ref());
                let mut remainder = lvalue.clone();
                remainder.values.remove(0);
                remainder.separators.remove(0);

                let mut deep_overrides = HashMap::new();
                deep_overrides.insert(
                    remainder.as_str(content.as_ref()).to_string(),
                    (out.clone(), b.assignment.right),
                );

                out.add_deep_overrides(override_target.to_string(), &deep_overrides);
            } else {
                out.inner.try_lock().unwrap().unresolved_identifiers.insert(
                    lvalue.as_str(content.as_ref()).to_string(),
                    b.assignment.right,
                );
            }
        }

        if let Some(value) = module.value {
            out.inner
                .try_lock()
                .unwrap()
                .unresolved_identifiers
                .insert(String::new(), value);
        }
        out
    }

    pub fn from_dictionary(dict: ast::Dictionary, content: Cow<'a, str>) -> Self {
        let (start, end) = dict.range();
        let out = Self::empty(content.clone(), start, end);
        for b in dict.values.values {
            let lvalue = b.left;
            if lvalue.values.len() > 1 {
                let override_target = lvalue.values[0].as_str(content.as_ref());
                let mut remainder = lvalue.clone();
                remainder.values.remove(0);
                remainder.separators.remove(0);

                let mut deep_overrides = HashMap::new();
                deep_overrides.insert(
                    remainder.as_str(content.as_ref()).to_string(),
                    (out.clone(), b.right),
                );

                out.add_deep_overrides(override_target.to_string(), &deep_overrides);
            } else {
                out.inner
                    .try_lock()
                    .unwrap()
                    .unresolved_identifiers
                    .insert(lvalue.as_str(content.as_ref()).to_string(), b.right);
            }
        }
        out
    }

    pub fn resolve_scope(&self, ident: &str, offset: usize) -> Result<Scope<'a>, ExecError> {
        if let Some(s) = self.inner.try_lock().unwrap().scopes.get(ident) {
            return Ok(s.clone());
        }

        match self.partially_resolve(ident, offset)? {
            ValueOrScope::Value(v) => {
                return Err(ExecError::CannotResolveSymbol(ParseError::from_string(
                    format!("unable to access inside of this (it's {})", v.type_name()),
                    "",
                    offset,
                    offset + ident.len(),
                )))
            }
            ValueOrScope::Scope(s) => return Ok(s),
        };
    }

    pub fn keys(&self) -> Vec<String> {
        let mut out = HashSet::new();
        let overrides: Vec<Scope<'a>> = self
            .inner
            .try_lock()
            .unwrap()
            .overrides
            .iter()
            .map(|s| s.to_owned())
            .collect();
        for or in overrides {
            for key in or.keys() {
                out.insert(key);
            }
        }

        for (k, _) in self.inner.try_lock().unwrap().unresolved_identifiers.iter() {
            out.insert(k.to_string());
        }
        let mut out: Vec<_> = out.into_iter().collect();
        out.sort_unstable();
        out
    }

    pub fn resolve(&self, specifier: &str, offset: usize) -> Result<Value, ExecError> {
        let out = self
            .partially_resolve(specifier, offset)
            .map(|vos| match vos {
                ValueOrScope::Value(v) => Ok(v),
                ValueOrScope::Scope(s) => self.resolve_scope_value(specifier, s),
            });
        match out {
            Ok(r) => r,
            Err(e) => Err(e),
        }
    }

    pub fn resolve_scope_value(
        &self,
        specifier: &str,
        scope: Scope<'a>,
    ) -> Result<Value, ExecError> {
        let scope = if let Some(o) = self.inner.try_lock().unwrap().deep_overrides.get(specifier) {
            let updated = scope.duplicate();
            for (key, (scope, expr)) in o.iter() {
                let mut components_iter = key.split(".");
                let first = components_iter.next().unwrap_or("").to_string();
                let rest = components_iter.collect::<Vec<_>>().join(".");

                let mut entry = HashMap::new();
                entry.insert(rest, (scope.clone(), expr.clone()));
                updated.add_deep_overrides(first, &entry);
            }
            updated
        } else {
            scope
        };

        let mut out = Dictionary::new();
        for key in scope.keys() {
            let value = scope.resolve(&key, 0)?;
            out.insert(key, value);
        }

        Ok(Value::Dictionary(out))
    }

    pub fn partially_resolve(
        &self,
        specifier: &str,
        offset: usize,
    ) -> Result<ValueOrScope<'a>, ExecError> {
        if let Some(idx) = specifier.find('.') {
            let prefix = &specifier[..idx];
            let suffix = &specifier[idx + 1..];

            let s = self.resolve_scope(prefix, offset)?;
            return s.partially_resolve(suffix, offset + idx);
        }

        if !self
            .inner
            .try_lock()
            .unwrap()
            .in_progress_identifiers
            .insert(specifier.to_string())
        {
            return Err(ExecError::CannotResolveSymbol(ParseError::from_string(
                format!("circular dependency when resolving `{}`", specifier),
                "",
                offset,
                offset + specifier.len(),
            )));
        }

        if self
            .inner
            .try_lock()
            .unwrap()
            .deep_overrides
            .get(specifier)
            .is_some()
        {
            let maybe_expression = {
                let inner = self.inner.try_lock().unwrap();
                let o = inner.deep_overrides.get(specifier).unwrap();

                if let Some((scope, expr)) = o.get("") {
                    Some((scope.clone(), expr.clone()))
                } else {
                    None
                }
            };

            if let Some((scope, expr)) = maybe_expression {
                let result = scope.evaluate_expression(specifier, &expr);
                self.inner
                    .try_lock()
                    .unwrap()
                    .in_progress_identifiers
                    .remove(specifier);
                return result;
            }
        }

        let overrides: Vec<Scope<'a>> = self
            .inner
            .try_lock()
            .unwrap()
            .overrides
            .iter()
            .map(|s| s.to_owned())
            .collect();
        for scope in overrides {
            let result = scope.partially_resolve(specifier, offset);
            if let Err(ExecError::CannotResolveSymbol(_)) = scope.resolve(specifier, offset) {
                continue;
            }

            self.inner
                .try_lock()
                .unwrap()
                .in_progress_identifiers
                .remove(specifier);

            return result;
        }

        self.inner
            .try_lock()
            .unwrap()
            .in_progress_identifiers
            .remove(specifier);

        if let Some(value) = self
            .inner
            .try_lock()
            .unwrap()
            .resolved_identifiers
            .get(specifier)
        {
            return Ok(value.clone());
        }

        let expression = {
            let _inner = self.inner.try_lock().unwrap();
            _inner
                .unresolved_identifiers
                .get(specifier)
                .map(|expr| expr.clone())
        };
        if let Some(expr) = expression {
            return self.evaluate_expression(specifier, &expr);
        }

        let parent = {
            let _inner = self.inner.try_lock().unwrap();
            _inner.parent_scope.as_ref().map(|s| s.clone())
        };
        if let Some(p) = parent {
            return p.partially_resolve(specifier, offset);
        }

        if specifier.is_empty() {
            let mut out = Dictionary::new();
            for key in self.keys() {
                let value = match self.resolve(&key, 0) {
                    Ok(v) => v,
                    Err(e) => return Err(e),
                };
                out.insert(key, value);
            }
            return Ok(ValueOrScope::Value(Value::Dictionary(out)));
        }

        {
            let _inner = self.inner.try_lock().unwrap();
            if let Some(import) = _inner.unresolved_imports.get(specifier) {
                for (idx, resolver) in _inner.import_resolvers.iter().enumerate() {
                    let res = resolver.resolve_import(
                        import.as_str(),
                        _inner.import_context.as_ref().map(|s| s.as_str()),
                    );

                    let import_resolution = match res {
                        Ok(ir) => ir,
                        Err(e) => {
                            if idx == _inner.import_resolvers.len() - 1 {
                                return Err(e);
                            }
                            continue;
                        }
                    };

                    let tmp = Scope::from_module(
                        import_resolution.module,
                        import_resolution.content.into(),
                    );
                    tmp.add_import_resolvers(_inner.import_resolvers.clone());
                    tmp.set_import_context(import_resolution.context);

                    return match import {
                        ImportedIdentifier::Direct(_) => tmp.partially_resolve(specifier, 0),
                        ImportedIdentifier::Aliased(_) => tmp.partially_resolve("", 0),
                    };
                }
            };
        };

        Err(ExecError::CannotResolveSymbol(ParseError::from_string(
            format!("unable to resolve identifier `{}`", specifier),
            "",
            offset,
            offset + specifier.len(),
        )))
    }

    pub fn evaluate_expression(
        &self,
        specifier: &str,
        expr: &ast::Expression,
    ) -> Result<ValueOrScope<'a>, ExecError> {
        let content = self.inner.try_lock().unwrap().content.clone();
        let deps = eval::get_dependencies(expr);
        let mut resolved_dependencies = HashMap::new();

        self.inner
            .try_lock()
            .unwrap()
            .in_progress_identifiers
            .insert(specifier.to_string());

        for d in deps {
            let name = d.as_str(content.as_ref());
            let (start, _) = d.range();
            let resolved = self.partially_resolve(name, start)?;
            resolved_dependencies.insert(name.to_string(), resolved.clone());
            self.inner
                .try_lock()
                .unwrap()
                .resolved_identifiers
                .insert(name.to_string(), resolved);
        }

        self.inner
            .try_lock()
            .unwrap()
            .in_progress_identifiers
            .remove(specifier);

        eval::evaluate(expr, content, &resolved_dependencies, self)
    }

    pub fn add_import_resolvers(&self, resolvers: Vec<Arc<dyn ImportResolver>>) {
        self.inner.lock().unwrap().import_resolvers = resolvers;
    }

    pub fn set_import_context(&self, context: Option<String>) {
        self.inner.lock().unwrap().import_context = context;
    }

    pub fn set_parent(&self, parent: &Scope<'a>) {
        self.inner.lock().unwrap().parent_scope = Some(parent.clone());
    }
}

pub fn exec_with_import_resolvers(
    module: ast::Module,
    content: &str,
    specifier: &str,
    resolvers: Vec<Arc<dyn ImportResolver>>,
) -> Result<Value, ExecError> {
    exec_with_import_resolvers_and_context(module, content, specifier, resolvers, None)
}

pub fn exec_with_import_resolvers_and_context(
    module: ast::Module,
    content: &str,
    specifier: &str,
    resolvers: Vec<Arc<dyn ImportResolver>>,
    import_context: Option<String>,
) -> Result<Value, ExecError> {
    let root = Scope::from_module(module, content.into());
    root.add_import_resolvers(resolvers);
    root.set_import_context(import_context);
    root.resolve(specifier, 0)
}

pub fn exec(module: ast::Module, content: &str, specifier: &str) -> Result<Value, ExecError> {
    let root = Scope::from_module(module, content.into());
    root.resolve(specifier, 0)
}
