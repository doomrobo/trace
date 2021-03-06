#![feature(quote, plugin_registrar, rustc_private)]

extern crate rustc_plugin;
extern crate syntax;

use std::collections::HashSet;

use rustc_plugin::Registry;

use syntax::ast::ExprKind::Lit;
use syntax::ast::ItemKind::{Fn, Impl, Mod, Static};
use syntax::ast::LitKind::{Int, Str};
use syntax::ast::MetaItemKind::{List, NameValue, Word};
use syntax::ast::Mutability::Mutable;
use syntax::ast::{
    self, Block, FnDecl, Ident, ImplItem, ImplItemKind, Item, ItemKind, MetaItem,
    NestedMetaItemKind, PatKind,
};
use syntax::ext::base::SyntaxExtension::MultiModifier;
use syntax::ext::base::{Annotatable, ExtCtxt};
use syntax::ext::build::AstBuilder;
use syntax::parse::token;
use syntax::ptr::P;
use syntax::source_map::{self, Span};
use syntax::symbol::Symbol;
use syntax::tokenstream::TokenTree;

#[plugin_registrar]
pub fn registrar(reg: &mut Registry) {
    reg.register_syntax_extension(
        Symbol::intern("trace"),
        MultiModifier(Box::new(trace_expand)),
    );
}

fn trace_expand(
    cx: &mut ExtCtxt,
    sp: Span,
    meta: &MetaItem,
    annotatable: Annotatable,
) -> Annotatable {
    let options = get_options(cx, meta);
    match annotatable {
        Annotatable::Item(item) => {
            let res = match item.node {
                Fn(..) => {
                    let new_item = expand_function(cx, options, &item, true);
                    cx.item(item.span, item.ident, item.attrs.clone(), new_item)
                        .map(|mut it| {
                            it.vis = item.vis.clone();
                            it
                        })
                }
                Mod(ref m) => {
                    let new_items = expand_mod(cx, m, options);
                    cx.item(
                        item.span,
                        item.ident,
                        item.attrs.clone(),
                        Mod(ast::Mod {
                            inner: m.inner,
                            items: new_items,
                            inline: m.inline,
                        }),
                    )
                }
                Impl(
                    safety,
                    polarity,
                    defaultness,
                    ref generics,
                    ref traitref,
                    ref ty,
                    ref items,
                ) => {
                    let new_items = expand_impl(cx, &*items, options);
                    cx.item(
                        item.span,
                        item.ident,
                        item.attrs.clone(),
                        Impl(
                            safety,
                            polarity,
                            defaultness,
                            generics.clone(),
                            traitref.clone(),
                            ty.clone(),
                            new_items,
                        ),
                    )
                }
                _ => {
                    cx.span_err(sp, "trace is only permissible on functions, mods, or impls");
                    item.clone()
                }
            };
            Annotatable::Item(res)
        }
        Annotatable::ImplItem(item) => {
            let new_item = expand_impl_method(cx, options, &item, true);
            Annotatable::ImplItem(P(ImplItem {
                node: new_item,
                attrs: vec![],
                ..(*item).clone()
            }))
        }
        Annotatable::TraitItem(_) => {
            cx.span_err(sp, "trace is not applicable to trait items");
            annotatable.clone()
        }
        Annotatable::ForeignItem(_) => {
            cx.span_err(sp, "trace is not applicable to foreign items");
            annotatable.clone()
        }
        Annotatable::Stmt(_) => {
            cx.span_err(sp, "trace is not applicable to statements");
            annotatable.clone()
        }
        Annotatable::Expr(_) => {
            cx.span_err(sp, "trace is not applicable to expressions");
            annotatable.clone()
        }
    }
}

#[derive(Clone)]
struct Options {
    prefix_enter: String,
    prefix_exit: String,
    enable: Option<HashSet<String>>,
    disable: Option<HashSet<String>>,
    pause: bool,
}

impl Options {
    fn new() -> Options {
        Options {
            prefix_enter: "[+]".to_string(),
            prefix_exit: "[-]".to_string(),
            enable: None,
            disable: None,
            pause: false,
        }
    }
}

fn get_options(cx: &mut ExtCtxt, meta: &MetaItem) -> Options {
    fn meta_list_to_set(cx: &mut ExtCtxt, list: &[&MetaItem]) -> HashSet<String> {
        let mut v = HashSet::new();
        for item in list {
            match item.node {
                Word => {
                    v.insert(item.name().to_string());
                }
                List(_) | NameValue(_) => {
                    cx.span_warn(item.span, &format!("Invalid option {}", item.name()))
                }
            }
        }
        v
    }

    let mut options = Options::new();
    if let List(ref v) = meta.node {
        for i in v {
            if let NestedMetaItemKind::MetaItem(ref mi) = i.node {
                match mi.node {
                    NameValue(ref s) => {
                        if mi.name() == "prefix_enter" {
                            if let Str(ref new_prefix, _) = s.node {
                                options.prefix_enter = new_prefix.to_string();
                            }
                        } else if mi.name() == "prefix_exit" {
                            if let Str(ref new_prefix, _) = s.node {
                                options.prefix_exit = new_prefix.to_string();
                            }
                        } else {
                            cx.span_warn(i.span, &format!("Invalid option {}", mi.name()));
                        }
                    }
                    List(ref list) => {
                        let list: Vec<_> = list
                            .iter()
                            .filter_map(|x| {
                                if let NestedMetaItemKind::MetaItem(ref mi) = x.node {
                                    Some(&(*mi))
                                } else {
                                    None
                                }
                            })
                            .collect();
                        if mi.name() == "enable" {
                            options.enable = Some(meta_list_to_set(cx, &list[..]));
                        } else if mi.name() == "disable" {
                            options.disable = Some(meta_list_to_set(cx, &list[..]));
                        } else {
                            cx.span_warn(i.span, &format!("Invalid option {}", mi.name()));
                        }
                    }
                    Word => {
                        if mi.name() == "pause" {
                            options.pause = true;
                        } else {
                            cx.span_warn(i.span, &format!("Invalid option {}", mi.name()))
                        }
                    }
                }
            }
        }
    }
    if options.enable.is_some() && options.disable.is_some() {
        cx.span_err(
            meta.span,
            "Cannot use both enable and disable options with trace",
        );
    }
    options
}

fn expand_impl(cx: &mut ExtCtxt, items: &[ImplItem], options: Options) -> Vec<ImplItem> {
    let mut new_items = vec![];
    for item in items.iter() {
        if let ImplItemKind::Method(..) = item.node {
            let new_item = expand_impl_method(cx, options.clone(), item, false);
            new_items.push(ImplItem {
                node: new_item,
                attrs: vec![],
                ..(*item).clone()
            });
        }
    }
    new_items
}

fn expand_impl_method(
    cx: &mut ExtCtxt,
    options: Options,
    item: &ImplItem,
    direct: bool,
) -> ImplItemKind {
    let name = &*item.ident.name.as_str();

    // If the attribute is not directly on this method, we filter by function names
    if !direct {
        match (&options.enable, &options.disable) {
            (&Some(ref s), &None) => {
                if !s.contains(name) {
                    return item.node.clone();
                }
            }
            (&None, &Some(ref s)) => {
                if s.contains(name) {
                    return item.node.clone();
                }
            }
            (&Some(_), &Some(_)) => unreachable!(),
            _ => (),
        }
    }

    if let ImplItemKind::Method(ref sig, ref block) = item.node {
        let idents = arg_idents(cx, &sig.decl);
        let new_block = new_block(cx, options, name, block.clone(), idents, direct);
        ImplItemKind::Method(sig.clone(), new_block)
    } else {
        panic!("Expected method");
    }
}

fn expand_mod(cx: &mut ExtCtxt, m: &ast::Mod, options: Options) -> Vec<P<Item>> {
    let mut new_items = vec![];
    let mut depth_correct = false;
    let mut depth_span = None;
    for i in m.items.iter() {
        match i.node {
            Fn(..) => {
                let new_item = expand_function(cx, options.clone(), i, false);
                new_items.push(cx.item(i.span, i.ident, i.attrs.clone(), new_item));
            }
            Static(_, ref mut_, ref expr) => {
                let name = &i.ident.name.as_str();
                if *name == Symbol::intern("DEPTH").as_str() {
                    depth_span = Some(i.span);
                    if let &Mutable = mut_ {
                        if let Lit(ref lit) = expr.node {
                            if let Int(ref val, _) = lit.node {
                                if *val == 0 {
                                    depth_correct = true;
                                }
                            }
                        }
                    }
                }
                new_items.push((*i).clone());
            }
            Impl(safety, polarity, defaultness, ref generics, ref traitref, ref ty, ref items) => {
                let new_impl_items = expand_impl(cx, &**items, options.clone());
                new_items.push(cx.item(
                    i.span,
                    i.ident,
                    i.attrs.clone(),
                    Impl(
                        safety,
                        polarity,
                        defaultness,
                        generics.clone(),
                        traitref.clone(),
                        ty.clone(),
                        new_impl_items,
                    ),
                ));
            }
            _ => {
                new_items.push((*i).clone());
            }
        }
    }
    if let Some(sp) = depth_span {
        if !depth_correct {
            cx.span_err(
                sp,
                "A static variable with the name `DEPTH` was found, but either the \
                 mutability, the type, or the inital value are incorrect",
            );
        }
    } else {
        let depth_ident = Ident::with_empty_ctxt(Symbol::intern("DEPTH"));
        let u32_ident = Ident::with_empty_ctxt(Symbol::intern("u32"));
        let ty = cx.ty_path(cx.path(source_map::DUMMY_SP, vec![u32_ident]));
        let item_ = cx.item_static(
            source_map::DUMMY_SP,
            depth_ident,
            ty,
            Mutable,
            cx.expr_u32(source_map::DUMMY_SP, 0),
        );
        new_items.push(item_);
    }

    new_items
}

fn expand_function(cx: &mut ExtCtxt, options: Options, item: &P<Item>, direct: bool) -> ItemKind {
    let name = &&*item.ident.name.as_str();

    // If the attribute is not directly on this method, we filter by function names
    if !direct {
        match (&options.enable, &options.disable) {
            (&Some(ref s), &None) | (&None, &Some(ref s)) => {
                if !s.contains(*name) {
                    return item.node.clone();
                }
            }
            (&Some(_), &Some(_)) => unreachable!(),
            _ => (),
        }
    }

    if let Fn(ref decl, ref header, ref generics, ref block) = item.node {
        let idents = arg_idents(cx, &**decl);
        let new_block = new_block(cx, options, name, block.clone(), idents, direct);
        Fn(decl.clone(), header.clone(), generics.clone(), new_block)
    } else {
        panic!("Expected a function")
    }
}

fn arg_idents(cx: &mut ExtCtxt, decl: &FnDecl) -> Vec<Ident> {
    fn extract_idents(cx: &mut ExtCtxt, pat: &ast::PatKind, idents: &mut Vec<Ident>) {
        match *pat {
            PatKind::Paren(..)
            | PatKind::Wild
            | PatKind::TupleStruct(_, _, None)
            | PatKind::Lit(_)
            | PatKind::Range(..)
            | PatKind::Path(..) => (),
            PatKind::Ident(_, sp, _) => {
                if &*sp.name.as_str() != "self" {
                    idents.push(sp);
                }
            }
            PatKind::TupleStruct(_, ref v, _) | PatKind::Tuple(ref v, _) => {
                for p in v {
                    extract_idents(cx, &p.node, idents);
                }
            }
            PatKind::Struct(_, ref v, _) => {
                for p in v {
                    extract_idents(cx, &p.node.pat.node, idents);
                }
            }
            PatKind::Slice(ref v1, ref opt, ref v2) => {
                for p in v1 {
                    extract_idents(cx, &p.node, idents);
                }
                if let &Some(ref p) = opt {
                    extract_idents(cx, &p.node, idents);
                }
                for p in v2 {
                    extract_idents(cx, &p.node, idents);
                }
            }
            PatKind::Box(ref p) | PatKind::Ref(ref p, _) => extract_idents(cx, &p.node, idents),
            PatKind::Mac(ref m) => {
                let sp = m.node.path.span;
                cx.span_warn(sp, "trace ignores pattern macros in function arguments");
            }
        }
    }
    let mut idents = vec![];
    for arg in decl.inputs.iter() {
        extract_idents(cx, &arg.pat.node, &mut idents);
    }
    idents
}

fn new_block(
    cx: &mut ExtCtxt,
    options: Options,
    name: &str,
    block: P<Block>,
    idents: Vec<Ident>,
    direct: bool,
) -> P<Block> {
    // If the attribute is on this method, we filter the arguments
    let idents = if direct {
        match (&options.enable, &options.disable) {
            (&Some(ref s), &None) => idents
                .into_iter()
                .filter(|x| s.contains(&*x.name.as_str()))
                .collect(),
            (&None, &Some(ref s)) => idents
                .into_iter()
                .filter(|x| !s.contains(&*x.name.as_str()))
                .collect(),
            (&Some(_), &Some(_)) => unreachable!(),
            _ => idents,
        }
    } else {
        idents
    };

    let args: Vec<TokenTree> = idents
        .iter()
        .map(|&ident| vec![token::Ident(ident.clone(), false)])
        .collect::<Vec<_>>()
        .join(&token::Comma)
        .into_iter()
        .map(|t| TokenTree::Token(source_map::DUMMY_SP, t))
        .collect();

    let mut arg_fmt = vec![];
    for ident in idents.iter() {
        arg_fmt.push(format!("{}: {{:?}}", ident))
    }
    let arg_fmt_str = &*arg_fmt.join(", ");

    let prefix_enter = &*options.prefix_enter;
    let prefix_exit = &*options.prefix_exit;
    let pause = options.pause;

    let new_block = quote_expr!(cx,
        unsafe {
            let mut s = String::new();
            (0..DEPTH).map(|_| s.push(' ')).count();
            let args = format!($arg_fmt_str, $args);
            println!("{}{} Entering {}({})", s, $prefix_enter, $name, args);
            if $pause {
                use std::io::{BufRead, stdin};
                let stdin = stdin();
                stdin.lock().lines().next();
            }
            DEPTH += 1;
            let mut __trace_closure = move || $block;
            let __trace_result = __trace_closure();
            DEPTH -= 1;
            println!("{}{} Exiting {} = {:?}", s, $prefix_exit, $name, __trace_result);
            if $pause {
                use std::io::{BufRead, stdin};
                let stdin = stdin();
                stdin.lock().lines().next();
            }
            __trace_result
    });
    cx.block_expr(new_block)
}
