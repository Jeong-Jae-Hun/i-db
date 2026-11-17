//! 편집기의 SQL 자동 완성. 후보는 `vm::completion` 이 정하고 여기서는 LSP 꼴로 옮긴다.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{App, Task, Window};
use gpui_component::input::{CompletionProvider, Rope, RopeExt};
use idb_engine::Catalog;
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, Range, TextEdit};

use crate::vm::completion::{Kind, complete};

/// 탭의 접속 카탈로그다. 접속을 바꾸거나 카탈로그가 오면 화면이 갈아 끼운다.
pub type CatalogCell = Rc<RefCell<Option<Rc<Catalog>>>>;

pub struct SqlCompletion {
    pub catalog: CatalogCell,
}

impl CompletionProvider for SqlCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let Some(cat) = self.catalog.borrow().clone() else {
            return Task::ready(Ok(CompletionResponse::Array(vec![])));
        };
        let s = text.to_string();
        let (range, cands) = complete(&cat, &s, offset);
        let lsp_range = Range {
            start: text.offset_to_position(range.start),
            end: text.offset_to_position(range.end),
        };
        let items = cands
            .into_iter()
            .map(|c| CompletionItem {
                kind: Some(match c.kind {
                    Kind::Column => CompletionItemKind::FIELD,
                    Kind::Table => CompletionItemKind::STRUCT,
                    Kind::View => CompletionItemKind::INTERFACE,
                    Kind::Schema => CompletionItemKind::MODULE,
                    Kind::Function => CompletionItemKind::FUNCTION,
                    Kind::Keyword => CompletionItemKind::KEYWORD,
                }),
                detail: (!c.detail.is_empty()).then_some(c.detail),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: lsp_range,
                    new_text: c.label.clone(),
                })),
                label: c.label,
                ..Default::default()
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        new_text.chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
    }
}
