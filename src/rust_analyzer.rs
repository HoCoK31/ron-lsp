use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use syn::{Attribute, Fields, Item, ItemEnum, ItemStruct, ItemType, Type};
use tokio::sync::RwLock;
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct FieldInfo {
    pub name: String,
    pub type_name: String,
    pub docs: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub has_default: bool,
}

impl FieldInfo {
    pub fn is_optional(&self) -> bool {
        self.type_name.starts_with("Option") || self.has_default
    }
}

#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub name: String,
    pub fields: Vec<FieldInfo>,
    pub docs: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

#[derive(Debug, Clone)]
pub enum TypeKind {
    Struct(Vec<FieldInfo>),
    Enum(Vec<EnumVariant>),
}

#[derive(Debug, Clone)]
pub struct TypeInfo {
    pub name: String,
    pub kind: TypeKind,
    pub docs: Option<String>,
    pub source_file: Option<PathBuf>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub has_default: bool,
}

impl TypeInfo {
    pub fn fields(&self) -> Option<&Vec<FieldInfo>> {
        match &self.kind {
            TypeKind::Struct(fields) => Some(fields),
            TypeKind::Enum(_) => None,
        }
    }

    pub fn find_field(&self, field_name: &str) -> Option<&FieldInfo> {
        match &self.kind {
            TypeKind::Struct(fields) => fields.iter().find(|f| f.name == field_name),
            TypeKind::Enum(variants) => {
                // Search through all variants' fields
                variants
                    .iter()
                    .flat_map(|v| &v.fields)
                    .find(|f| f.name == field_name)
            }
        }
    }

    pub fn find_variant(&self, variant_name: &str) -> Option<&EnumVariant> {
        match &self.kind {
            TypeKind::Enum(variants) => variants.iter().find(|v| v.name == variant_name),
            TypeKind::Struct(_) => None,
        }
    }
}

pub struct RustAnalyzer {
    workspace_root: RwLock<Option<PathBuf>>,
    type_cache: RwLock<HashMap<String, TypeInfo>>,
    type_aliases: RwLock<HashMap<String, String>>,
    initial_scan_complete: RwLock<bool>,
    custom_deserializers: RwLock<HashSet<String>>,
}

impl RustAnalyzer {
    pub fn new() -> Self {
        Self {
            workspace_root: RwLock::new(None),
            type_cache: RwLock::new(HashMap::new()),
            type_aliases: RwLock::new(HashMap::new()),
            initial_scan_complete: RwLock::new(false),
            custom_deserializers: RwLock::new(HashSet::new()),
        }
    }

    pub async fn set_workspace_root(&self, root: &Path) {
        *self.workspace_root.write().await = Some(root.to_path_buf());
        // Trigger initial scan
        self.scan_workspace().await;
        // Mark initial scan as complete
        *self.initial_scan_complete.write().await = true;
    }

    pub async fn scan_workspace(&self) {
        let root = {
            let guard = self.workspace_root.read().await;
            match guard.as_ref() {
                Some(r) => r.clone(),
                None => return,
            }
        };

        let mut type_cache = self.type_cache.write().await;
        let mut type_aliases = self.type_aliases.write().await;

        let mut custom_deserializers = HashSet::new();

        // Find all .rs files in the workspace
        for entry in WalkDir::new(&root)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("rs"))
        {
            if let Ok(content) = fs::read_to_string(entry.path()) {
                if let Ok(syntax_tree) = syn::parse_file(&content) {
                    collect_custom_deserializers(
                        &syntax_tree.items,
                        &self.file_path_to_module_path(entry.path()),
                        &mut custom_deserializers,
                    );
                    self.extract_types_from_file(
                        &syntax_tree,
                        entry.path(),
                        &mut type_cache,
                        &mut type_aliases,
                    );
                }
            }
        }
        *self.custom_deserializers.write().await = custom_deserializers;
    }

    fn extract_types_from_file(
        &self,
        syntax_tree: &syn::File,
        file_path: &Path,
        type_cache: &mut HashMap<String, TypeInfo>,
        type_aliases: &mut HashMap<String, String>,
    ) {
        // Extract module path from file path
        let module_prefix = self.file_path_to_module_path(file_path);

        for item in &syntax_tree.items {
            if let Item::Struct(struct_item) = item {
                if let Some(type_info) =
                    self.extract_struct_info(struct_item, &module_prefix, file_path)
                {
                    type_cache.insert(type_info.name.clone(), type_info);
                }
            } else if let Item::Enum(enum_item) = item {
                if let Some(type_info) =
                    self.extract_enum_info(enum_item, &module_prefix, file_path)
                {
                    type_cache.insert(type_info.name.clone(), type_info);
                }
            } else if let Item::Type(type_item) = item {
                self.extract_type_alias(type_item, &module_prefix, type_aliases);
            } else if let Item::Mod(mod_item) = item {
                // Handle inline modules
                if let Some((_, items)) = &mod_item.content {
                    let mod_name = mod_item.ident.to_string();
                    let nested_prefix = if module_prefix.is_empty() {
                        mod_name
                    } else {
                        format!("{}::{}", module_prefix, mod_name)
                    };

                    for item in items {
                        if let Item::Struct(struct_item) = item {
                            if let Some(type_info) =
                                self.extract_struct_info(struct_item, &nested_prefix, file_path)
                            {
                                type_cache.insert(type_info.name.clone(), type_info);
                            }
                        } else if let Item::Enum(enum_item) = item {
                            if let Some(type_info) =
                                self.extract_enum_info(enum_item, &nested_prefix, file_path)
                            {
                                type_cache.insert(type_info.name.clone(), type_info);
                            }
                        } else if let Item::Type(type_item) = item {
                            self.extract_type_alias(type_item, &nested_prefix, type_aliases);
                        }
                    }
                }
            }
        }
    }

    /// Convert a file path to a module path
    ///
    /// # Examples
    /// ```
    /// let module_path = self.file_file_path_to_module_path("src/models/user.rs");
    /// assert_eq!(module_path, "crate::models::user");
    /// ```
    fn file_path_to_module_path(&self, file_path: &Path) -> String {
        let components: Vec<_> = file_path
            .components()
            .filter_map(|c| match c {
                Component::Normal(os) => os.to_str(),
                _ => None,
            })
            .collect();

        if let Some(src_index) = components.iter().position(|c| *c == "src") {
            let relative = &components[src_index + 1..];
            if relative.is_empty() {
                return "crate".to_string();
            }

            let mut parts = relative.to_vec();
            if let Some(last) = parts.last_mut() {
                if last.ends_with(".rs") {
                    let stem = last.trim_end_matches(".rs");
                    *last = stem;
                }
                if *last == "lib" || *last == "main" {
                    parts.pop();
                    if parts.is_empty() {
                        return "crate".to_string();
                    }
                } else if *last == "mod" {
                    parts.pop();
                }
            }

            if parts.is_empty() {
                "crate".to_string()
            } else {
                format!("crate::{}", parts.join("::"))
            }
        } else {
            String::new()
        }
    }

    fn extract_struct_info(
        &self,
        struct_item: &ItemStruct,
        module_prefix: &str,
        file_path: &Path,
    ) -> Option<TypeInfo> {
        let struct_name = struct_item.ident.to_string();
        let full_path = if module_prefix.is_empty() {
            struct_name.clone()
        } else {
            format!("{}::{}", module_prefix, struct_name)
        };

        let docs = extract_docs(&struct_item.attrs);
        let rename_all = serde_attributes::extract_rename_all(&struct_item.attrs);

        let fields = match &struct_item.fields {
            Fields::Named(fields) => fields
                .named
                .iter()
                .map(|field| {
                    let field_attributes = serde_attributes::extract_serde_field_attributes(field);
                    let has_default = field_attributes.has_default;
                    let field_name = serde_attributes::ron_visible_name(
                        &field.ident.as_ref().unwrap().to_string(),
                        field_attributes.rename.as_deref(),
                        rename_all.as_deref(),
                    );
                    let type_name = type_to_string(&field.ty);
                    let field_docs = extract_docs(&field.attrs);
                    let (line, column) = field
                        .ident
                        .as_ref()
                        .map(|i| {
                            let start = i.span().start();
                            (start.line, start.column)
                        })
                        .unzip();

                    FieldInfo {
                        name: field_name,
                        type_name,
                        docs: field_docs,
                        line,
                        column,
                        has_default,
                    }
                })
                .collect(),
            Fields::Unnamed(fields) => fields
                .unnamed
                .iter()
                .enumerate()
                .map(|(i, field)| {
                    let type_name = type_to_string(&field.ty);

                    let field_attributes = serde_attributes::extract_serde_field_attributes(field);
                    let has_default = field_attributes.has_default;

                    FieldInfo {
                        name: i.to_string(),
                        type_name,
                        docs: None,
                        line: None,
                        column: None,
                        has_default,
                    }
                })
                .collect(),
            Fields::Unit => vec![],
        };

        let start = struct_item.ident.span().start();
        let line = Some(start.line);
        let column = Some(start.column);
        let has_default = has_default_derive(&struct_item.attrs);

        Some(TypeInfo {
            name: full_path,
            kind: TypeKind::Struct(fields),
            docs,
            source_file: Some(file_path.to_path_buf()),
            line,
            column,
            has_default,
        })
    }

    fn extract_enum_info(
        &self,
        enum_item: &ItemEnum,
        module_prefix: &str,
        file_path: &Path,
    ) -> Option<TypeInfo> {
        let enum_name = enum_item.ident.to_string();
        let full_path = if module_prefix.is_empty() {
            enum_name.clone()
        } else {
            format!("{}::{}", module_prefix, enum_name)
        };

        let docs = extract_docs(&enum_item.attrs);
        let rename_all = serde_attributes::extract_rename_all(&enum_item.attrs);

        let variants = enum_item
            .variants
            .iter()
            .map(|variant| {
                let variant_attributes = serde_attributes::extract_serde_attributes(&variant.attrs);
                let variant_name = serde_attributes::ron_visible_name(
                    &variant.ident.to_string(),
                    variant_attributes.rename.as_deref(),
                    rename_all.as_deref(),
                );
                let variant_docs = extract_docs(&variant.attrs);
                let variant_start = variant.ident.span().start();
                let variant_line = Some(variant_start.line);
                let variant_column = Some(variant_start.column);

                let fields = match &variant.fields {
                    Fields::Named(fields) => fields
                        .named
                        .iter()
                        .map(|field| {
                            let field_attributes =
                                serde_attributes::extract_serde_field_attributes(field);
                            let has_default = field_attributes.has_default;
                            let field_name = serde_attributes::ron_visible_name(
                                &field.ident.as_ref().unwrap().to_string(),
                                field_attributes.rename.as_deref(),
                                rename_all.as_deref(),
                            );
                            let type_name = type_to_string(&field.ty);
                            let field_docs = extract_docs(&field.attrs);
                            let (line, column) = field
                                .ident
                                .as_ref()
                                .map(|i| {
                                    let start = i.span().start();
                                    (start.line, start.column)
                                })
                                .unzip();

                            FieldInfo {
                                name: field_name,
                                type_name,
                                docs: field_docs,
                                line,
                                column,
                                has_default,
                            }
                        })
                        .collect(),
                    Fields::Unnamed(fields) => fields
                        .unnamed
                        .iter()
                        .enumerate()
                        .map(|(i, field)| {
                            let type_name = type_to_string(&field.ty);

                            let field_attributes =
                                serde_attributes::extract_serde_field_attributes(field);
                            let has_default = field_attributes.has_default;

                            FieldInfo {
                                name: i.to_string(),
                                type_name,
                                docs: None,
                                line: None,
                                column: None,
                                has_default,
                            }
                        })
                        .collect(),
                    Fields::Unit => vec![],
                };

                EnumVariant {
                    name: variant_name,
                    fields,
                    docs: variant_docs,
                    line: variant_line,
                    column: variant_column,
                }
            })
            .collect();

        let start = enum_item.ident.span().start();
        let line = Some(start.line);
        let column = Some(start.column);
        let has_default = has_default_derive(&enum_item.attrs);

        Some(TypeInfo {
            name: full_path,
            kind: TypeKind::Enum(variants),
            docs,
            source_file: Some(file_path.to_path_buf()),
            line,
            column,
            has_default,
        })
    }

    fn extract_type_alias(
        &self,
        type_item: &ItemType,
        module_prefix: &str,
        type_aliases: &mut HashMap<String, String>,
    ) {
        let alias_name = type_item.ident.to_string();
        let full_alias_path = if module_prefix.is_empty() {
            alias_name.clone()
        } else {
            format!("{}::{}", module_prefix, alias_name)
        };

        let target_type = type_to_string(&type_item.ty);

        type_aliases.insert(full_alias_path, target_type);
    }

    pub async fn get_type_info(&self, type_path: &str) -> Option<TypeInfo> {
        // Resolve type aliases first
        let resolved_type = {
            let aliases = self.type_aliases.read().await;
            aliases
                .get(type_path)
                .cloned()
                .unwrap_or_else(|| type_path.to_string())
        };

        // First check cache with exact match
        {
            let cache = self.type_cache.read().await;
            if let Some(info) = cache.get(&resolved_type) {
                return Some(info.clone());
            }
            // Also try the original type path
            if let Some(info) = cache.get(type_path) {
                return Some(info.clone());
            }

            // If not found by exact match, try finding by simple name
            // e.g., "PostType" should match "crate::models::PostType"
            // After resolving alias, also try simple name match for resolved_type
            for (key, value) in cache.iter() {
                if key.ends_with(&format!("::{}", type_path))
                    || key.ends_with(&format!("::{}", resolved_type))
                    || key == type_path
                    || key == &resolved_type
                {
                    return Some(value.clone());
                }
            }

            // Тип мог переехать в подмодуль: `content::AbilityDef` находит
            // `content::types::AbilityDef`, если совпадает последний сегмент пути.
            let wanted = type_path.rsplit("::").next().unwrap_or(type_path);
            if !wanted.is_empty() {
                if let Some((_, value)) = cache
                    .iter()
                    .find(|(key, _)| key.rsplit("::").next() == Some(wanted))
                {
                    return Some(value.clone());
                }
            }
        }

        // If not in cache and initial scan hasn't completed, rescan
        let scan_complete = *self.initial_scan_complete.read().await;
        if !scan_complete {
            self.scan_workspace().await;

            let cache = self.type_cache.read().await;
            return cache
                .get(&resolved_type)
                .cloned()
                .or_else(|| cache.get(type_path).cloned())
                .or_else(|| {
                    // Try simple name match after rescan
                    for (key, value) in cache.iter() {
                        if key.ends_with(&format!("::{}", type_path)) || key == type_path {
                            return Some(value.clone());
                        }
                    }
                    None
                });
        }

        // Initial scan already complete, type not found
        None
    }

    /// Custom Deserialize implementations and Serde conversions define their own
    /// wire format. Static field layout cannot validate that representation.
    pub async fn has_custom_deserializer(&self, type_name: &str) -> bool {
        let mut inner = type_name.replace(' ', "");
        let mut visited = HashSet::new();
        while visited.insert(inner.clone()) {
            let wrapper = ["Option<", "Vec<", "Box<", "Rc<", "Arc<"]
                .into_iter()
                .find(|prefix| inner.starts_with(prefix));
            if let Some(prefix) = wrapper.filter(|_| inner.ends_with('>')) {
                inner = inner[prefix.len()..inner.len() - 1].to_string();
                continue;
            }
            let aliases = self.type_aliases.read().await;
            let target = aliases.get(&inner).or_else(|| {
                let mut matches = aliases
                    .iter()
                    .filter(|(name, _)| name.ends_with(&format!("::{inner}")));
                let (_, target) = matches.next()?;
                if matches.next().is_none() {
                    Some(target)
                } else {
                    None
                }
            });
            match target {
                Some(target) => inner = target.replace(' ', ""),
                None => break,
            }
        }
        if let Some(info) = self.get_type_info(&inner).await {
            return self.custom_deserializers.read().await.contains(&info.name);
        }
        false
    }

    /// Get all types from the workspace
    pub async fn get_all_types(&self) -> Vec<TypeInfo> {
        let cache = self.type_cache.read().await;
        cache.values().cloned().collect()
    }

    #[cfg(test)]
    pub async fn insert_type_for_test(&self, type_info: TypeInfo) {
        let mut cache = self.type_cache.write().await;
        cache.insert(type_info.name.clone(), type_info);
    }
}

fn extract_docs(attrs: &[Attribute]) -> Option<String> {
    let docs: Vec<String> = attrs
        .iter()
        .filter_map(|attr| {
            if attr.path().is_ident("doc") {
                attr.meta.require_name_value().ok().and_then(|nv| {
                    if let syn::Expr::Lit(lit) = &nv.value {
                        if let syn::Lit::Str(s) = &lit.lit {
                            return Some(s.value().trim().to_string());
                        }
                    }
                    None
                })
            } else {
                None
            }
        })
        .collect();

    if docs.is_empty() {
        None
    } else {
        Some(docs.join("\n"))
    }
}

fn has_default_derive(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if attr.path().is_ident("derive") {
            // Parse the derive attribute to check for Default
            if let Ok(meta_list) = attr.meta.require_list() {
                let tokens_str = meta_list.tokens.to_string();
                // Check if "Default" appears in the derive list
                tokens_str.split(',').any(|s| s.trim() == "Default")
            } else {
                false
            }
        } else {
            false
        }
    })
}

fn type_to_string(ty: &Type) -> String {
    match ty {
        Type::Path(type_path) => quote::quote!(#type_path).to_string(),
        Type::Reference(type_ref) => {
            let inner = type_to_string(&type_ref.elem);
            if type_ref.mutability.is_some() {
                format!("&mut {}", inner)
            } else {
                format!("&{}", inner)
            }
        }
        _ => quote::quote!(#ty).to_string(),
    }
}

mod serde_attributes {
    use syn::{Attribute, Field};

    /// Serde attributes of a field (or variant): what shapes the RON name and optionality.
    pub struct SerdeFieldAttributes {
        pub has_default: bool,
        pub rename: Option<String>,
    }

    pub fn extract_serde_field_attributes(field: &Field) -> SerdeFieldAttributes {
        extract_serde_attributes(&field.attrs)
    }

    /// Reads `#[serde(default)]` and `#[serde(rename = "…")]`.
    ///
    /// The `rename(serialize = "…", deserialize = "…")` form is not parsed: it is
    /// rare, and ignoring it is safe here (the name stays the Rust identifier, as
    /// it was before this change).
    pub fn extract_serde_attributes(attrs: &[Attribute]) -> SerdeFieldAttributes {
        let mut field_attrs = SerdeFieldAttributes {
            has_default: false,
            rename: None,
        };

        let attrs = attrs.iter().filter(|attr| attr.path().is_ident("serde"));

        for attr in attrs {
            let _ = attr.parse_nested_meta(|meta| {
                // #[serde(default)]
                if meta.path.is_ident("default") {
                    field_attrs.has_default = true;
                }
                // #[serde(rename = "type")]
                if meta.path.is_ident("rename") {
                    if let Ok(value) = meta.value() {
                        if let Ok(lit) = value.parse::<syn::LitStr>() {
                            field_attrs.rename = Some(lit.value());
                        }
                    }
                }
                Ok(())
            });
        }

        field_attrs
    }

    /// The container's `#[serde(rename_all = "…")]` rule, when present.
    pub fn extract_rename_all(attrs: &[Attribute]) -> Option<String> {
        let mut rule = None;

        for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
            let _ = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("rename_all") {
                    if let Ok(value) = meta.value() {
                        if let Ok(lit) = value.parse::<syn::LitStr>() {
                            rule = Some(lit.value());
                        }
                    }
                }
                Ok(())
            });
        }

        rule
    }

    /// The name a field or variant is visible under in RON.
    ///
    /// A `r#` prefix is stripped (Rust keywords are written without it in RON) and
    /// an explicit `rename` wins over the container's `rename_all` rule.
    pub fn ron_visible_name(
        rust_name: &str,
        rename: Option<&str>,
        rename_all: Option<&str>,
    ) -> String {
        let base = rust_name.strip_prefix("r#").unwrap_or(rust_name);

        if let Some(rename) = rename {
            return rename.to_string();
        }

        match rename_all {
            Some(rule) => apply_rename_rule(rule, base),
            None => base.to_string(),
        }
    }

    /// Splits a name into words, supporting `snake_case`, `kebab-case` and
    /// `camelCase`/`PascalCase`.
    fn words(name: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut prev_lower = false;

        for ch in name.chars() {
            if ch == '_' || ch == '-' || ch == ' ' {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                prev_lower = false;
                continue;
            }

            if ch.is_uppercase() && prev_lower && !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }

            prev_lower = ch.is_lowercase() || ch.is_ascii_digit();
            current.push(ch);
        }

        if !current.is_empty() {
            out.push(current);
        }

        out
    }

    fn capitalize(word: &str) -> String {
        let mut chars = word.chars();
        match chars.next() {
            Some(first) => {
                first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
            }
            None => String::new(),
        }
    }

    /// Applies a serde `rename_all` rule; an unknown rule keeps the name as is.
    pub fn apply_rename_rule(rule: &str, name: &str) -> String {
        let parts = words(name);
        if parts.is_empty() {
            return name.to_string();
        }

        let joined = |sep: &str, f: fn(&str) -> String| -> String {
            parts.iter().map(|w| f(w)).collect::<Vec<_>>().join(sep)
        };

        match rule {
            "lowercase" => parts.join("").to_lowercase(),
            "UPPERCASE" => parts.join("").to_uppercase(),
            "PascalCase" => parts
                .iter()
                .map(|w| capitalize(w))
                .collect::<Vec<_>>()
                .join(""),
            "camelCase" => {
                let mut iter = parts.iter();
                let first = iter.next().map(|w| w.to_lowercase()).unwrap_or_default();
                first + &iter.map(|w| capitalize(w)).collect::<String>()
            }
            "snake_case" => joined("_", |w| w.to_lowercase()),
            "SCREAMING_SNAKE_CASE" => joined("_", |w| w.to_uppercase()),
            "kebab-case" => joined("-", |w| w.to_lowercase()),
            "SCREAMING-KEBAB-CASE" => joined("-", |w| w.to_uppercase()),
            _ => name.to_string(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn raw_ident_loses_the_prefix() {
            assert_eq!(ron_visible_name("r#mod", None, None), "mod");
            assert_eq!(ron_visible_name("r#type", None, None), "type");
        }

        #[test]
        fn explicit_rename_wins_over_rename_all() {
            assert_eq!(
                ron_visible_name("ty", Some("type"), Some("snake_case")),
                "type"
            );
        }

        #[test]
        fn rename_all_rules_cover_the_serde_set() {
            assert_eq!(apply_rename_rule("snake_case", "someField"), "some_field");
            assert_eq!(apply_rename_rule("camelCase", "some_field"), "someField");
            assert_eq!(apply_rename_rule("PascalCase", "some_field"), "SomeField");
            assert_eq!(apply_rename_rule("kebab-case", "someField"), "some-field");
            assert_eq!(
                apply_rename_rule("SCREAMING_SNAKE_CASE", "someField"),
                "SOME_FIELD"
            );
            assert_eq!(apply_rename_rule("UPPERCASE", "some_field"), "SOMEFIELD");
            assert_eq!(apply_rename_rule("lowercase", "SomeField"), "somefield");
            assert_eq!(
                apply_rename_rule("SCREAMING-KEBAB-CASE", "someField"),
                "SOME-FIELD"
            );
            // Unknown rule: keep the name as is
            assert_eq!(apply_rename_rule("nonsense", "someField"), "someField");
        }

        #[test]
        fn words_split_on_separators_and_lower_to_upper_steps() {
            assert_eq!(words("someField"), vec!["some", "Field"]);
            assert_eq!(words("some_field_name"), vec!["some", "field", "name"]);
            assert_eq!(words("kebab-case-name"), vec!["kebab", "case", "name"]);
            // Digits count as lowercase, so the next capital still splits.
            assert_eq!(words("field2Name"), vec!["field2", "Name"]);
            // A run of capitals is not split, so it stays a single word.
            assert_eq!(words("HTTPServer"), vec!["HTTPServer"]);
            assert_eq!(words(""), Vec::<String>::new());
        }

        #[test]
        fn capitalize_keeps_the_tail_lowercase() {
            assert_eq!(capitalize("hTTP"), "Http");
            assert_eq!(capitalize(""), "");
        }
    }
}

fn collect_custom_deserializers(items: &[Item], prefix: &str, result: &mut HashSet<String>) {
    let qualify = |name: &str| {
        if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}::{name}")
        }
    };
    for item in items {
        match item {
            Item::Impl(item)
                if item.trait_.as_ref().is_some_and(|(_, path, _)| {
                    path.segments
                        .last()
                        .is_some_and(|segment| segment.ident == "Deserialize")
                }) =>
            {
                if let Type::Path(ty) = item.self_ty.as_ref() {
                    let name = ty
                        .path
                        .segments
                        .iter()
                        .map(|s| s.ident.to_string())
                        .collect::<Vec<_>>()
                        .join("::");
                    result.insert(if name.starts_with("crate::") {
                        name
                    } else {
                        qualify(&name)
                    });
                }
            }
            Item::Struct(item) if has_serde_conversion(&item.attrs) => {
                result.insert(qualify(&item.ident.to_string()));
            }
            Item::Enum(item) if has_serde_conversion(&item.attrs) => {
                result.insert(qualify(&item.ident.to_string()));
            }
            Item::Mod(item) => {
                if let Some((_, items)) = &item.content {
                    collect_custom_deserializers(items, &qualify(&item.ident.to_string()), result);
                }
            }
            _ => {}
        }
    }
}

fn has_serde_conversion(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("serde"))
        .any(|attr| {
            let mut custom = false;
            let _ = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("transparent")
                    || meta.path.is_ident("from")
                    || meta.path.is_ident("try_from")
                {
                    custom = true;
                }
                // Consume unrelated attribute values so later options are visited.
                if meta.input.peek(syn::Token![=]) {
                    let _: syn::Expr = meta.value()?.parse()?;
                } else if meta.input.peek(syn::token::Paren) {
                    let content;
                    syn::parenthesized!(content in meta.input);
                    let _: proc_macro2::TokenStream = content.parse()?;
                }
                Ok(())
            });
            custom
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::{parse_file, parse_str, Field};

    fn field(name: &str, type_name: &str) -> FieldInfo {
        FieldInfo {
            name: name.to_string(),
            type_name: type_name.to_string(),
            docs: None,
            line: None,
            column: None,
            has_default: false,
        }
    }

    fn struct_type(name: &str, fields: Vec<FieldInfo>, has_default: bool) -> TypeInfo {
        TypeInfo {
            name: name.to_string(),
            kind: TypeKind::Struct(fields),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default,
        }
    }

    fn enum_type(name: &str, variants: Vec<EnumVariant>) -> TypeInfo {
        TypeInfo {
            name: name.to_string(),
            kind: TypeKind::Enum(variants),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        }
    }

    fn variant(name: &str, fields: Vec<FieldInfo>) -> EnumVariant {
        EnumVariant {
            name: name.to_string(),
            fields,
            docs: None,
            line: None,
            column: None,
        }
    }

    fn parse_struct(source: &str) -> ItemStruct {
        parse_str(source).expect("parsable struct")
    }

    fn parse_enum(source: &str) -> ItemEnum {
        parse_str(source).expect("parsable enum")
    }

    fn first_field(item: &ItemStruct) -> Field {
        match &item.fields {
            Fields::Named(fields) => fields.named.first().expect("named field").clone(),
            _ => panic!("expected named fields"),
        }
    }

    #[test]
    fn is_optional_follows_the_option_prefix_and_the_default_flag() {
        assert!(field("value", "Option<u32>").is_optional());
        assert!(field("value", "Option").is_optional());
        // Only the prefix is checked, so any type starting with it counts.
        assert!(field("value", "Optional<u32>").is_optional());
        assert!(!field("value", "u32").is_optional());

        let mut with_default = field("value", "u32");
        with_default.has_default = true;
        assert!(with_default.is_optional());
    }

    #[test]
    fn fields_are_available_for_structs_only() {
        let item = struct_type(
            "Item",
            vec![field("id", "u32"), field("name", "String")],
            false,
        );
        assert_eq!(item.fields().map(|fields| fields.len()), Some(2));

        assert!(enum_type("Kind", vec![]).fields().is_none());
    }

    #[test]
    fn find_field_looks_into_struct_and_variant_fields() {
        let item = struct_type("Item", vec![field("id", "u32")], false);
        assert_eq!(
            item.find_field("id").map(|found| found.type_name.as_str()),
            Some("u32")
        );
        assert!(item.find_field("missing").is_none());

        let kind = enum_type(
            "Kind",
            vec![
                variant("First", vec![]),
                variant("Second", vec![field("length", "u32")]),
            ],
        );
        assert_eq!(
            kind.find_field("length")
                .map(|found| found.type_name.as_str()),
            Some("u32")
        );
        assert!(kind.find_field("id").is_none());
    }

    #[test]
    fn find_variant_is_available_for_enums_only() {
        let kind = enum_type("Kind", vec![variant("First", vec![])]);
        assert_eq!(
            kind.find_variant("First").map(|found| found.name.as_str()),
            Some("First")
        );
        assert!(kind.find_variant("Second").is_none());

        let item = struct_type("Item", vec![], false);
        assert!(item.find_variant("Item").is_none());
    }

    #[test]
    fn file_path_to_module_path_drops_src_and_module_file_names() {
        let analyzer = RustAnalyzer::new();

        assert_eq!(
            analyzer.file_path_to_module_path(Path::new("src/models/item.rs")),
            "crate::models::item"
        );
        // `lib`, `main` and `mod` name their parent module.
        assert_eq!(
            analyzer.file_path_to_module_path(Path::new("src/lib.rs")),
            "crate"
        );
        assert_eq!(
            analyzer.file_path_to_module_path(Path::new("src/main.rs")),
            "crate"
        );
        assert_eq!(
            analyzer.file_path_to_module_path(Path::new("src/models/mod.rs")),
            "crate::models"
        );
        assert_eq!(
            analyzer.file_path_to_module_path(Path::new("src/models/item/mod.rs")),
            "crate::models::item"
        );
        // No `src` component: the file is outside the crate root.
        assert_eq!(
            analyzer.file_path_to_module_path(Path::new("models/item.rs")),
            ""
        );
        assert_eq!(analyzer.file_path_to_module_path(Path::new("src")), "crate");
    }

    #[test]
    fn type_to_string_keeps_the_token_spacing_of_paths() {
        let render = |source: &str| type_to_string(&parse_str::<Type>(source).unwrap());

        assert_eq!(render("u32"), "u32");
        assert_eq!(render("Vec<Item>"), "Vec < Item >");
        assert_eq!(render("HashMap<String, u32>"), "HashMap < String , u32 >");
        assert_eq!(render("Option<Item>"), "Option < Item >");
    }

    #[test]
    fn type_to_string_rebuilds_references_and_falls_back_to_tokens() {
        let render = |source: &str| type_to_string(&parse_str::<Type>(source).unwrap());

        // References are rebuilt by hand, so no spacing is added inside them.
        assert_eq!(render("&Item"), "&Item");
        assert_eq!(render("&mut Item"), "&mut Item");
        assert_eq!(render("&'static str"), "&str");
        // Every other type is printed as its token stream.
        assert_eq!(render("[u8; 4]"), "[u8 ; 4]");
        assert_eq!(render("(u32, u8)"), "(u32 , u8)");
        assert_eq!(render("fn(u32) -> u32"), "fn (u32) -> u32");
    }

    #[test]
    fn extract_docs_joins_trimmed_doc_lines() {
        let item = parse_struct(
            "/// First line.\n///\n/// Second line.\n#[derive(Default)]\nstruct Item;",
        );
        assert_eq!(
            extract_docs(&item.attrs),
            Some("First line.\n\nSecond line.".to_string())
        );

        // `#[doc(hidden)]` and `#[doc = include_str!(…)]` are not name/value
        // pairs and are skipped instead of being reported as text.
        let undocumented = parse_struct("#[doc(hidden)] struct Item;");
        assert_eq!(extract_docs(&undocumented.attrs), None);
    }

    #[test]
    fn has_default_derive_matches_a_whole_token_only() {
        assert!(has_default_derive(
            &parse_struct("#[derive(Debug, Default)] struct Item;").attrs
        ));
        assert!(has_default_derive(
            &parse_struct("#[derive(Default,)] struct Item;").attrs
        ));
        assert!(!has_default_derive(
            &parse_struct("#[derive(Debug, DefaultClone)] struct Item;").attrs
        ));
        assert!(!has_default_derive(&parse_struct("struct Item;").attrs));
    }

    #[test]
    fn extract_serde_attributes_reads_default_and_rename() {
        let item = parse_struct("struct Item { #[serde(default, rename = \"kind\")] value: u32 }");
        let attrs = serde_attributes::extract_serde_field_attributes(&first_field(&item));
        assert!(attrs.has_default);
        assert_eq!(attrs.rename.as_deref(), Some("kind"));

        let plain = parse_struct("struct Item { #[serde(other)] value: u32 }");
        let attrs = serde_attributes::extract_serde_field_attributes(&first_field(&plain));
        assert!(!attrs.has_default);
        assert_eq!(attrs.rename, None);

        let container = parse_struct("#[serde(default)] struct Item;");
        let attrs = serde_attributes::extract_serde_attributes(&container.attrs);
        assert!(attrs.has_default);
        assert_eq!(attrs.rename, None);
    }

    #[test]
    fn serde_values_that_nothing_parses_stop_the_attribute_walk() {
        // The walker expects a comma after every key, so a `key = value` pair it
        // does not parse aborts the rest of the attribute list.
        let skipped = parse_struct(
            "struct Item { #[serde(skip_serializing_if = \"Option::is_none\", default)] value: Option<u32> }",
        );
        assert!(
            !serde_attributes::extract_serde_field_attributes(&first_field(&skipped)).has_default
        );

        let ordered = parse_struct(
            "struct Item { #[serde(default, skip_serializing_if = \"Option::is_none\")] value: Option<u32> }",
        );
        assert!(
            serde_attributes::extract_serde_field_attributes(&first_field(&ordered)).has_default
        );
    }

    #[test]
    fn extract_rename_all_reads_the_container_rule() {
        let renamed = parse_struct("#[serde(rename_all = \"camelCase\")] struct Item;");
        assert_eq!(
            serde_attributes::extract_rename_all(&renamed.attrs).as_deref(),
            Some("camelCase")
        );

        let plain = parse_struct("#[serde(default)] struct Item;");
        assert_eq!(serde_attributes::extract_rename_all(&plain.attrs), None);
    }

    #[test]
    fn has_serde_conversion_consumes_values_and_nested_options() {
        assert!(has_serde_conversion(
            &parse_struct("#[serde(from = \"Raw\")] struct Item;").attrs
        ));
        assert!(has_serde_conversion(
            &parse_struct("#[serde(transparent)] struct Item;").attrs
        ));
        assert!(has_serde_conversion(
            &parse_struct("#[serde(try_from = \"Raw\")] struct Item;").attrs
        ));
        // Values and parenthesised options are consumed, so the options after
        // them are still visited (unlike the serde name/optionality parser).
        assert!(has_serde_conversion(
            &parse_struct("#[serde(rename_all = \"snake_case\", transparent)] struct Item;").attrs
        ));
        assert!(has_serde_conversion(
            &parse_struct(
                "#[serde(bound(deserialize = \"T: Deserialize\"), transparent)] struct Item;"
            )
            .attrs
        ));
        assert!(!has_serde_conversion(
            &parse_struct("#[serde(rename_all = \"snake_case\")] struct Item;").attrs
        ));
    }

    #[test]
    fn collect_custom_deserializers_qualifies_names_and_walks_modules() {
        let file = parse_file(
            r#"
#[serde(from = "Raw")]
pub struct Converted { pub value: String }

#[serde(transparent)]
pub struct Transparent(pub String);

impl<'de> Deserialize<'de> for Manual {
    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error> {
        unimplemented!()
    }
}

impl<'de> Deserialize<'de> for crate::model::Remote {
    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error> {
        unimplemented!()
    }
}

pub mod nested {
    #[serde(try_from = "Raw")]
    pub struct Inner { pub id: u32 }
}
"#,
        )
        .unwrap();

        let mut names = HashSet::new();
        collect_custom_deserializers(&file.items, "", &mut names);

        // An already absolute `crate::` path stays as written.
        assert!(names.contains("crate::model::Remote"));
        assert!(names.contains("Converted"));
        assert!(names.contains("Transparent"));
        assert!(names.contains("Manual"));
        // Inline modules are walked with the module name as a prefix.
        assert!(names.contains("nested::Inner"));
        assert_eq!(names.len(), 5);

        let mut qualified = HashSet::new();
        collect_custom_deserializers(&file.items, "crate::model", &mut qualified);

        assert!(qualified.contains("crate::model::Converted"));
        assert!(qualified.contains("crate::model::nested::Inner"));
        assert_eq!(qualified.len(), 5);
    }

    #[test]
    fn extract_type_alias_records_the_full_path_and_the_target() {
        let analyzer = RustAnalyzer::new();
        let item: ItemType = parse_str("type ItemList = Vec<Item>;").unwrap();

        let mut aliases = HashMap::new();
        analyzer.extract_type_alias(&item, "crate::models", &mut aliases);
        assert_eq!(
            aliases.get("crate::models::ItemList").map(String::as_str),
            Some("Vec < Item >")
        );

        let mut root_aliases = HashMap::new();
        analyzer.extract_type_alias(&item, "", &mut root_aliases);
        assert_eq!(
            root_aliases.get("ItemList").map(String::as_str),
            Some("Vec < Item >")
        );
    }

    #[test]
    fn extract_types_from_file_walks_inline_modules_and_aliases() {
        let analyzer = RustAnalyzer::new();
        let file = parse_file(
            "struct Item { id: u32 }
enum Kind { First }
type ItemList = Vec<Item>;
mod nested { struct Inner { id: u32 } enum Nest { A } type Alias = u8; }
",
        )
        .unwrap();

        let mut cache = HashMap::new();
        let mut aliases = HashMap::new();
        analyzer.extract_types_from_file(
            &file,
            Path::new("src/models.rs"),
            &mut cache,
            &mut aliases,
        );

        let mut names: Vec<String> = cache.keys().cloned().collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "crate::models::Item",
                "crate::models::Kind",
                "crate::models::nested::Inner",
                "crate::models::nested::Nest",
            ]
        );
        assert_eq!(
            cache["crate::models::Item"].source_file.as_deref(),
            Some(Path::new("src/models.rs"))
        );

        assert_eq!(
            aliases.get("crate::models::ItemList").map(String::as_str),
            Some("Vec < Item >")
        );
        assert_eq!(
            aliases
                .get("crate::models::nested::Alias")
                .map(String::as_str),
            Some("u8")
        );
    }

    #[test]
    fn extract_struct_info_applies_serde_names_and_docs() {
        let analyzer = RustAnalyzer::new();
        let item = parse_struct(
            "#[derive(Default)]
#[serde(rename_all = \"snake_case\")]
/// Docs.
struct Item {
    /// Field docs.
    itemKind: u32,
    #[serde(default)]
    fallback: String,
}",
        );

        let info = analyzer
            .extract_struct_info(&item, "crate::models", Path::new("src/models.rs"))
            .expect("struct info");

        assert_eq!(info.name, "crate::models::Item");
        assert_eq!(info.docs.as_deref(), Some("Docs."));
        assert_eq!(
            info.source_file.as_deref(),
            Some(Path::new("src/models.rs"))
        );
        assert!(info.has_default);
        assert_eq!((info.line, info.column), (Some(4), Some(7)));

        let fields = info.fields().expect("struct fields");
        assert_eq!(fields.len(), 2);

        assert_eq!(fields[0].name, "item_kind");
        assert_eq!(fields[0].type_name, "u32");
        assert_eq!(fields[0].docs.as_deref(), Some("Field docs."));
        assert_eq!((fields[0].line, fields[0].column), (Some(6), Some(4)));
        assert!(!fields[0].has_default);

        assert_eq!(fields[1].name, "fallback");
        assert_eq!(fields[1].type_name, "String");
        assert!(fields[1].has_default);
    }

    #[test]
    fn extract_struct_info_numbers_unnamed_fields() {
        let analyzer = RustAnalyzer::new();
        let item = parse_struct("struct Pair(u32, String);");

        let info = analyzer
            .extract_struct_info(&item, "", Path::new("src/pair.rs"))
            .expect("struct info");

        assert_eq!(info.name, "Pair");
        let fields = info.fields().expect("struct fields");
        assert_eq!(
            fields
                .iter()
                .map(|field| (field.name.as_str(), field.type_name.as_str()))
                .collect::<Vec<_>>(),
            vec![("0", "u32"), ("1", "String")]
        );
        // Tuple fields carry neither docs nor a source position.
        assert_eq!((fields[0].docs.as_deref(), fields[0].line), (None, None));

        let unit = parse_struct("struct Unit;");
        let info = analyzer
            .extract_struct_info(&unit, "", Path::new("src/unit.rs"))
            .expect("struct info");
        assert!(info.fields().expect("struct fields").is_empty());
    }

    #[test]
    fn extract_enum_info_names_variants_and_their_fields() {
        let analyzer = RustAnalyzer::new();
        let item = parse_enum(
            "#[serde(rename_all = \"snake_case\")]
enum Kind {
    /// Unit docs.
    Unit,
    Named { itemKind: u32 },
    Tuple(u8, u16),
}",
        );

        let info = analyzer
            .extract_enum_info(&item, "crate::models", Path::new("src/models.rs"))
            .expect("enum info");

        assert_eq!(info.name, "crate::models::Kind");
        assert!(info.fields().is_none());

        let TypeKind::Enum(variants) = &info.kind else {
            panic!("expected an enum");
        };
        assert_eq!(
            variants.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            vec!["unit", "named", "tuple"]
        );
        assert_eq!(variants[0].docs.as_deref(), Some("Unit docs."));
        assert!(variants[0].fields.is_empty());

        assert_eq!(variants[1].fields[0].name, "item_kind");
        assert_eq!(variants[1].fields[0].type_name, "u32");

        assert_eq!(
            variants[2]
                .fields
                .iter()
                .map(|field| (field.name.as_str(), field.type_name.as_str()))
                .collect::<Vec<_>>(),
            vec![("0", "u8"), ("1", "u16")]
        );
    }

    #[tokio::test]
    async fn get_type_info_matches_exact_and_simple_names() {
        let analyzer = RustAnalyzer::new();
        analyzer
            .insert_type_for_test(struct_type(
                "crate::models::Item",
                vec![field("id", "u32")],
                false,
            ))
            .await;

        for query in ["crate::models::Item", "Item", "models::Item"] {
            assert_eq!(
                analyzer.get_type_info(query).await.map(|info| info.name),
                Some("crate::models::Item".to_string()),
                "query {query}"
            );
        }

        assert!(analyzer.get_type_info("Missing").await.is_none());
    }

    #[tokio::test]
    async fn get_type_info_resolves_aliases() {
        let analyzer = RustAnalyzer::new();
        analyzer
            .insert_type_for_test(struct_type("crate::models::Item", vec![], false))
            .await;
        analyzer
            .type_aliases
            .write()
            .await
            .insert("ItemAlias".to_string(), "crate::models::Item".to_string());
        analyzer
            .type_aliases
            .write()
            .await
            .insert("ShortAlias".to_string(), "Item".to_string());

        assert_eq!(
            analyzer
                .get_type_info("ItemAlias")
                .await
                .map(|info| info.name),
            Some("crate::models::Item".to_string())
        );
        assert_eq!(
            analyzer
                .get_type_info("ShortAlias")
                .await
                .map(|info| info.name),
            Some("crate::models::Item".to_string())
        );
    }

    #[tokio::test]
    async fn get_all_types_returns_the_cache_contents() {
        let analyzer = RustAnalyzer::new();
        assert!(analyzer.get_all_types().await.is_empty());

        analyzer
            .insert_type_for_test(struct_type("Item", vec![], false))
            .await;

        let all = analyzer.get_all_types().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "Item");
    }

    #[tokio::test]
    async fn has_custom_deserializer_unwraps_wrappers_and_aliases() {
        let analyzer = RustAnalyzer::new();
        analyzer
            .insert_type_for_test(struct_type("crate::models::Wrapper", vec![], false))
            .await;
        analyzer
            .custom_deserializers
            .write()
            .await
            .insert("crate::models::Wrapper".to_string());

        // The cached name is reached by its simple name.
        assert!(analyzer.has_custom_deserializer("Wrapper").await);
        assert!(analyzer.has_custom_deserializer("Vec<Wrapper>").await);
        assert!(
            analyzer
                .has_custom_deserializer("Option<Box<Wrapper>>")
                .await
        );

        analyzer
            .type_aliases
            .write()
            .await
            .insert("WrapperAlias".to_string(), "Vec<Wrapper>".to_string());
        assert!(analyzer.has_custom_deserializer("WrapperAlias").await);

        assert!(!analyzer.has_custom_deserializer("Vec<Missing>").await);
        // A wrapper that is not closed is not unwrapped either.
        assert!(!analyzer.has_custom_deserializer("Vec<Wrapper").await);
    }

    #[tokio::test]
    async fn has_custom_deserializer_stops_on_alias_cycles() {
        let analyzer = RustAnalyzer::new();
        {
            let mut aliases = analyzer.type_aliases.write().await;
            aliases.insert("First".to_string(), "Second".to_string());
            aliases.insert("Second".to_string(), "First".to_string());
        }

        // The visited set keeps the alias chase from looping forever.
        assert!(!analyzer.has_custom_deserializer("First").await);
    }
}
