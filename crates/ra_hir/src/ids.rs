use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use ra_db::{FileId, salsa};
use ra_syntax::{TreeArc, SourceFile, AstNode, ast};
use mbe::MacroRules;

use crate::{
    Module, DefDatabase, AstId, FileAstId,
};

/// hir makes heavy use of ids: integer (u32) handlers to various things. You
/// can think of id as a pointer (but without a lifetime) or a file descriptor
/// (but for hir objects).
///
/// This module defines a bunch of ids we are using. The most important ones are
/// probably `HirFileId` and `DefId`.

/// Input to the analyzer is a set of files, where each file is identified by
/// `FileId` and contains source code. However, another source of source code in
/// Rust are macros: each macro can be thought of as producing a "temporary
/// file". To assign an id to such a file, we use the id of the macro call that
/// produced the file. So, a `HirFileId` is either a `FileId` (source code
/// written by user), or a `MacroCallId` (source code produced by macro).
///
/// What is a `MacroCallId`? Simplifying, it's a `HirFileId` of a file
/// containing the call plus the offset of the macro call in the file. Note that
/// this is a recursive definition! However, the size_of of `HirFileId` is
/// finite (because everything bottoms out at the real `FileId`) and small
/// (`MacroCallId` uses the location interner).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HirFileId(HirFileIdRepr);

impl HirFileId {
    /// For macro-expansion files, returns the file original source file the
    /// expansion originated from.
    pub fn original_file(self, db: &impl DefDatabase) -> FileId {
        match self.0 {
            HirFileIdRepr::File(file_id) => file_id,
            HirFileIdRepr::Macro(macro_call_id) => {
                let loc = macro_call_id.loc(db);
                loc.ast_id.file_id().original_file(db)
            }
        }
    }

    /// XXX: this is a temporary function, which should go away when we implement the
    /// nameresolution+macro expansion combo. Prefer using `original_file` if
    /// possible.
    pub fn as_original_file(self) -> FileId {
        match self.0 {
            HirFileIdRepr::File(file_id) => file_id,
            HirFileIdRepr::Macro(_r) => panic!("macro generated file: {:?}", self),
        }
    }

    pub(crate) fn hir_parse_query(
        db: &impl DefDatabase,
        file_id: HirFileId,
    ) -> TreeArc<SourceFile> {
        match file_id.0 {
            HirFileIdRepr::File(file_id) => db.parse(file_id),
            HirFileIdRepr::Macro(macro_call_id) => {
                parse_macro(db, macro_call_id).unwrap_or_else(|err| {
                    // Note:
                    // The final goal we would like to make all parse_macro success,
                    // such that the following log will not call anyway.
                    log::warn!(
                        "fail on macro_parse: (reason: {}) {}",
                        err,
                        macro_call_id.debug_dump(db)
                    );

                    // returning an empty string looks fishy...
                    SourceFile::parse("")
                })
            }
        }
    }
}

fn parse_macro(
    db: &impl DefDatabase,
    macro_call_id: MacroCallId,
) -> Result<TreeArc<SourceFile>, String> {
    let loc = macro_call_id.loc(db);
    let macro_call = loc.ast_id.to_node(db);
    let (macro_arg, _) = macro_call
        .token_tree()
        .and_then(mbe::ast_to_token_tree)
        .ok_or("Fail to args in to tt::TokenTree")?;

    let macro_rules = db.macro_def(loc.def).ok_or("Fail to find macro definition")?;
    let tt = macro_rules.expand(&macro_arg).map_err(|err| format!("{:?}", err))?;

    // Set a hard limit for the expanded tt
    let count = tt.count();
    if count > 65536 {
        return Err(format!("Total tokens count exceed limit : count = {}", count));
    }

    Ok(mbe::token_tree_to_ast_item_list(&tt))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum HirFileIdRepr {
    File(FileId),
    Macro(MacroCallId),
}

impl From<FileId> for HirFileId {
    fn from(file_id: FileId) -> HirFileId {
        HirFileId(HirFileIdRepr::File(file_id))
    }
}

impl From<MacroCallId> for HirFileId {
    fn from(macro_call_id: MacroCallId) -> HirFileId {
        HirFileId(HirFileIdRepr::Macro(macro_call_id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacroDefId(pub(crate) AstId<ast::MacroCall>);

pub(crate) fn macro_def_query(db: &impl DefDatabase, id: MacroDefId) -> Option<Arc<MacroRules>> {
    let macro_call = id.0.to_node(db);
    let arg = macro_call.token_tree()?;
    let (tt, _) = mbe::ast_to_token_tree(arg).or_else(|| {
        log::warn!("fail on macro_def to token tree: {:#?}", arg);
        None
    })?;
    let rules = MacroRules::parse(&tt).ok().or_else(|| {
        log::warn!("fail on macro_def parse: {:#?}", tt);
        None
    })?;
    Some(Arc::new(rules))
}

macro_rules! impl_intern_key {
    ($name:ident) => {
        impl salsa::InternKey for $name {
            fn from_intern_id(v: salsa::InternId) -> Self {
                $name(v)
            }
            fn as_intern_id(&self) -> salsa::InternId {
                self.0
            }
        }
    };
}

/// `MacroCallId` identifies a particular macro invocation, like
/// `println!("Hello, {}", world)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacroCallId(salsa::InternId);
impl_intern_key!(MacroCallId);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MacroCallLoc {
    pub(crate) def: MacroDefId,
    pub(crate) ast_id: AstId<ast::MacroCall>,
}

impl MacroCallId {
    pub(crate) fn loc(self, db: &impl DefDatabase) -> MacroCallLoc {
        db.lookup_intern_macro(self)
    }
}

impl MacroCallLoc {
    pub(crate) fn id(self, db: &impl DefDatabase) -> MacroCallId {
        db.intern_macro(self)
    }
}

#[derive(Debug)]
pub struct ItemLoc<N: AstNode> {
    pub(crate) module: Module,
    ast_id: AstId<N>,
}

impl<N: AstNode> PartialEq for ItemLoc<N> {
    fn eq(&self, other: &Self) -> bool {
        self.module == other.module && self.ast_id == other.ast_id
    }
}
impl<N: AstNode> Eq for ItemLoc<N> {}
impl<N: AstNode> Hash for ItemLoc<N> {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        self.module.hash(hasher);
        self.ast_id.hash(hasher);
    }
}

impl<N: AstNode> Clone for ItemLoc<N> {
    fn clone(&self) -> ItemLoc<N> {
        ItemLoc { module: self.module, ast_id: self.ast_id }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct LocationCtx<DB> {
    db: DB,
    module: Module,
    file_id: HirFileId,
}

impl<'a, DB: DefDatabase> LocationCtx<&'a DB> {
    pub(crate) fn new(db: &'a DB, module: Module, file_id: HirFileId) -> LocationCtx<&'a DB> {
        LocationCtx { db, module, file_id }
    }
    pub(crate) fn to_def<N, DEF>(self, ast: &N) -> DEF
    where
        N: AstNode,
        DEF: AstItemDef<N>,
    {
        DEF::from_ast(self, ast)
    }
}

pub(crate) trait AstItemDef<N: AstNode>: salsa::InternKey + Clone {
    fn intern(db: &impl DefDatabase, loc: ItemLoc<N>) -> Self;
    fn lookup_intern(self, db: &impl DefDatabase) -> ItemLoc<N>;

    fn from_ast(ctx: LocationCtx<&impl DefDatabase>, ast: &N) -> Self {
        let items = ctx.db.ast_id_map(ctx.file_id);
        let item_id = items.ast_id(ast);
        Self::from_ast_id(ctx, item_id)
    }
    fn from_ast_id(ctx: LocationCtx<&impl DefDatabase>, ast_id: FileAstId<N>) -> Self {
        let loc = ItemLoc { module: ctx.module, ast_id: ast_id.with_file_id(ctx.file_id) };
        Self::intern(ctx.db, loc)
    }
    fn source(self, db: &impl DefDatabase) -> (HirFileId, TreeArc<N>) {
        let loc = self.lookup_intern(db);
        let ast = loc.ast_id.to_node(db);
        (loc.ast_id.file_id(), ast)
    }
    fn module(self, db: &impl DefDatabase) -> Module {
        let loc = self.lookup_intern(db);
        loc.module
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunctionId(salsa::InternId);
impl_intern_key!(FunctionId);

impl AstItemDef<ast::FnDef> for FunctionId {
    fn intern(db: &impl DefDatabase, loc: ItemLoc<ast::FnDef>) -> Self {
        db.intern_function(loc)
    }
    fn lookup_intern(self, db: &impl DefDatabase) -> ItemLoc<ast::FnDef> {
        db.lookup_intern_function(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StructId(salsa::InternId);
impl_intern_key!(StructId);
impl AstItemDef<ast::StructDef> for StructId {
    fn intern(db: &impl DefDatabase, loc: ItemLoc<ast::StructDef>) -> Self {
        db.intern_struct(loc)
    }
    fn lookup_intern(self, db: &impl DefDatabase) -> ItemLoc<ast::StructDef> {
        db.lookup_intern_struct(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EnumId(salsa::InternId);
impl_intern_key!(EnumId);
impl AstItemDef<ast::EnumDef> for EnumId {
    fn intern(db: &impl DefDatabase, loc: ItemLoc<ast::EnumDef>) -> Self {
        db.intern_enum(loc)
    }
    fn lookup_intern(self, db: &impl DefDatabase) -> ItemLoc<ast::EnumDef> {
        db.lookup_intern_enum(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConstId(salsa::InternId);
impl_intern_key!(ConstId);
impl AstItemDef<ast::ConstDef> for ConstId {
    fn intern(db: &impl DefDatabase, loc: ItemLoc<ast::ConstDef>) -> Self {
        db.intern_const(loc)
    }
    fn lookup_intern(self, db: &impl DefDatabase) -> ItemLoc<ast::ConstDef> {
        db.lookup_intern_const(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StaticId(salsa::InternId);
impl_intern_key!(StaticId);
impl AstItemDef<ast::StaticDef> for StaticId {
    fn intern(db: &impl DefDatabase, loc: ItemLoc<ast::StaticDef>) -> Self {
        db.intern_static(loc)
    }
    fn lookup_intern(self, db: &impl DefDatabase) -> ItemLoc<ast::StaticDef> {
        db.lookup_intern_static(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraitId(salsa::InternId);
impl_intern_key!(TraitId);
impl AstItemDef<ast::TraitDef> for TraitId {
    fn intern(db: &impl DefDatabase, loc: ItemLoc<ast::TraitDef>) -> Self {
        db.intern_trait(loc)
    }
    fn lookup_intern(self, db: &impl DefDatabase) -> ItemLoc<ast::TraitDef> {
        db.lookup_intern_trait(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeAliasId(salsa::InternId);
impl_intern_key!(TypeAliasId);
impl AstItemDef<ast::TypeAliasDef> for TypeAliasId {
    fn intern(db: &impl DefDatabase, loc: ItemLoc<ast::TypeAliasDef>) -> Self {
        db.intern_type_alias(loc)
    }
    fn lookup_intern(self, db: &impl DefDatabase) -> ItemLoc<ast::TypeAliasDef> {
        db.lookup_intern_type_alias(self)
    }
}

impl MacroCallId {
    pub fn debug_dump(&self, db: &impl DefDatabase) -> String {
        let loc = self.clone().loc(db);
        let node = loc.ast_id.to_node(db);
        let syntax_str = node.syntax().text().chunks().collect::<Vec<_>>().join(" ");

        // dump the file name
        let file_id: HirFileId = self.clone().into();
        let original = file_id.original_file(db);
        let macro_rules = db.macro_def(loc.def);

        format!(
            "macro call [file: {:#?}] : {}\nhas rules: {}",
            db.file_relative_path(original),
            syntax_str,
            macro_rules.is_some()
        )
    }
}
