use std::env;

use super::{
    settings_page::{
        render_sub_header_with_description, MatchData, PageType, SettingsPageEvent,
        SettingsPageMeta, SettingsPageViewHandle, SettingsWidget,
    },
    SettingsSection,
};
use crate::appearance::Appearance;
use warp_core::ui::theme::Fill;
use warpui::{
    clipboard::ClipboardContent,
    elements::{
        Container, CrossAxisAlignment, Element, Flex, MainAxisSize, MouseStateHandle, Padding,
        ParentElement, Text, Wrap,
    },
    fonts::{Properties, Weight},
    ui_components::{
        button::ButtonVariant,
        components::{Coords, UiComponent, UiComponentStyles},
    },
    AppContext, Entity, TypedActionView, View, ViewContext, ViewHandle,
};

const FIRECRAWL_URL: &str = "https://www.firecrawl.dev/";
const ANTHROPIC_KEYS_URL: &str = "https://console.anthropic.com/settings/keys";
const OPENAI_KEYS_URL: &str = "https://platform.openai.com/api-keys";
const OPENROUTER_KEYS_URL: &str = "https://openrouter.ai/keys";
const WEB_AGENT_REPO_URL: &str = "https://github.com/firecrawl/web-agent";
const DEEP_RESEARCH_REPO_URL: &str = "https://github.com/aloewright/open-deep-research";

const SETUP_SHELL_SNIPPET: &str = "cp tools/web-research/.env.example tools/web-research/.env\n${EDITOR:-vi} tools/web-research/.env\nset -a; source tools/web-research/.env; set +a";

const ENV_VARS: &[(&str, &str)] = &[
    (
        "FIRECRAWL_API_KEY",
        "Required. Powers search/scrape/extract.",
    ),
    (
        "ANTHROPIC_API_KEY",
        "Optional. Anthropic provider (default: claude-sonnet-4-6).",
    ),
    (
        "OPENAI_API_KEY",
        "Optional. OpenAI provider (chat + reasoning).",
    ),
    (
        "OPENROUTER_API_KEY",
        "Optional. OpenRouter (routes to many providers).",
    ),
];

#[derive(Debug, Clone)]
pub enum WebResearchPageAction {
    OpenUrl(String),
    CopySetupCommand,
}

pub struct WebResearchPageView {
    page: PageType<Self>,
}

impl WebResearchPageView {
    pub fn new(_ctx: &mut ViewContext<Self>) -> Self {
        Self {
            page: PageType::new_uncategorized(
                vec![
                    Box::new(WebResearchHeaderWidget),
                    Box::new(WebResearchSetupWidget::default()),
                    Box::new(WebResearchStatusWidget),
                    Box::new(WebResearchResourcesWidget::default()),
                ],
                None,
            ),
        }
    }
}

impl Entity for WebResearchPageView {
    type Event = SettingsPageEvent;
}

impl View for WebResearchPageView {
    fn ui_name() -> &'static str {
        "WebResearchPage"
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        self.page.render(self, app)
    }
}

impl TypedActionView for WebResearchPageView {
    type Action = WebResearchPageAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        match action {
            WebResearchPageAction::OpenUrl(url) => ctx.open_url(url),
            WebResearchPageAction::CopySetupCommand => {
                ctx.clipboard().write(ClipboardContent::plain_text(
                    SETUP_SHELL_SNIPPET.to_string(),
                ));
            }
        }
    }
}

impl SettingsPageMeta for WebResearchPageView {
    fn section() -> SettingsSection {
        SettingsSection::WebResearch
    }

    fn should_render(&self, _ctx: &AppContext) -> bool {
        true
    }

    fn update_filter(&mut self, query: &str, ctx: &mut ViewContext<Self>) -> MatchData {
        self.page.update_filter(query, ctx)
    }

    fn scroll_to_widget(&mut self, widget_id: &'static str) {
        self.page.scroll_to_widget(widget_id)
    }

    fn clear_highlighted_widget(&mut self) {
        self.page.clear_highlighted_widget();
    }
}

impl From<ViewHandle<WebResearchPageView>> for SettingsPageViewHandle {
    fn from(view_handle: ViewHandle<WebResearchPageView>) -> Self {
        SettingsPageViewHandle::WebResearch(view_handle)
    }
}

// ---------- widgets ----------

#[derive(Default)]
struct WebResearchHeaderWidget;

impl SettingsWidget for WebResearchHeaderWidget {
    type View = WebResearchPageView;

    fn search_terms(&self) -> &str {
        "web research firecrawl deep research agent overview"
    }

    fn render(
        &self,
        _view: &Self::View,
        appearance: &Appearance,
        _app: &AppContext,
    ) -> Box<dyn Element> {
        render_sub_header_with_description(
            appearance,
            "Web Research",
            "Two CLIs ship with this repo for agent-driven web research: \
             web-agent (a Firecrawl-powered tool-use loop) and deep-research \
             (an iterative search → extract → reason loop that produces a \
             markdown report). Both can be invoked from chat via /web-agent \
             and /deep-research, or by sub-agents.",
        )
    }
}

#[derive(Default)]
struct WebResearchSetupWidget {
    copy_button: MouseStateHandle,
    init_button: MouseStateHandle,
}

impl SettingsWidget for WebResearchSetupWidget {
    type View = WebResearchPageView;

    fn search_terms(&self) -> &str {
        "setup configure env api key firecrawl anthropic openai openrouter"
    }

    fn render(
        &self,
        _view: &Self::View,
        appearance: &Appearance,
        _app: &AppContext,
    ) -> Box<dyn Element> {
        let ui_builder = appearance.ui_builder();
        let theme = appearance.theme();

        let header = Text::new_inline("Setup", appearance.ui_font_family(), 14.)
            .with_style(Properties::default().weight(Weight::Bold))
            .with_color(theme.active_ui_text_color().into_solid())
            .finish();

        let description = ui_builder
            .paragraph(
                "1. Get a Firecrawl API key (free tier available).\n\
                 2. Get an LLM API key from at least one provider \
                 (Anthropic, OpenAI, or OpenRouter).\n\
                 3. Copy the setup command below, paste it in a Warp \
                 terminal opened at the repo root, and fill in the keys."
                    .to_string(),
            )
            .with_style(UiComponentStyles {
                font_color: Some(theme.nonactive_ui_text_color().into_solid()),
                font_size: Some(12.),
                margin: Some(Coords {
                    top: 4.,
                    bottom: 12.,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .build()
            .finish();

        let snippet_box = Container::new(
            Text::new_inline(SETUP_SHELL_SNIPPET, appearance.monospace_font_family(), 12.)
                .with_color(theme.active_ui_text_color().into_solid())
                .finish(),
        )
        .with_padding(Padding::uniform(10.))
        .with_margin_bottom(8.)
        .finish();

        let copy_button = ui_builder
            .button(ButtonVariant::Outlined, self.copy_button.clone())
            .with_text_label("Copy setup command".to_owned())
            .with_style(UiComponentStyles::default().set_padding(Coords {
                top: 6.,
                bottom: 6.,
                left: 12.,
                right: 12.,
            }))
            .build()
            .on_click(|ctx, _, _| {
                ctx.dispatch_typed_action(WebResearchPageAction::CopySetupCommand);
            })
            .finish();

        let firecrawl_button = ui_builder
            .button(ButtonVariant::Outlined, self.init_button.clone())
            .with_text_label("Get Firecrawl API key".to_owned())
            .with_style(UiComponentStyles::default().set_padding(Coords {
                top: 6.,
                bottom: 6.,
                left: 12.,
                right: 12.,
            }))
            .build()
            .on_click(|ctx, _, _| {
                ctx.dispatch_typed_action(WebResearchPageAction::OpenUrl(
                    FIRECRAWL_URL.to_string(),
                ));
            })
            .finish();

        let buttons = Wrap::row()
            .with_main_axis_size(MainAxisSize::Max)
            .with_children([
                Container::new(copy_button).with_margin_right(8.).finish(),
                firecrawl_button,
            ])
            .finish();

        Container::new(
            Flex::column()
                .with_cross_axis_alignment(CrossAxisAlignment::Start)
                .with_child(header)
                .with_child(description)
                .with_child(snippet_box)
                .with_child(buttons)
                .finish(),
        )
        .with_padding_bottom(20.)
        .finish()
    }
}

#[derive(Default)]
struct WebResearchStatusWidget;

impl SettingsWidget for WebResearchStatusWidget {
    type View = WebResearchPageView;

    fn search_terms(&self) -> &str {
        "status environment variables api keys firecrawl anthropic openai openrouter"
    }

    fn render(
        &self,
        _view: &Self::View,
        appearance: &Appearance,
        _app: &AppContext,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let ui_builder = appearance.ui_builder();

        let header = Text::new_inline("Environment status", appearance.ui_font_family(), 14.)
            .with_style(Properties::default().weight(Weight::Bold))
            .with_color(theme.active_ui_text_color().into_solid())
            .finish();

        let description = ui_builder
            .paragraph(
                "Detected from this Warp process's environment. Values \
                 sourced from tools/web-research/.env in a terminal will \
                 not show as configured here unless they were also exported \
                 before launching Warp."
                    .to_string(),
            )
            .with_style(UiComponentStyles {
                font_color: Some(theme.nonactive_ui_text_color().into_solid()),
                font_size: Some(12.),
                margin: Some(Coords {
                    top: 4.,
                    bottom: 8.,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .build()
            .finish();

        let mut rows = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Start);
        for (var_name, var_help) in ENV_VARS {
            let configured = env::var(var_name).map(|v| !v.is_empty()).unwrap_or(false);
            let badge_label = if configured { "Configured" } else { "Not set" };
            let badge_color = if configured {
                Fill::success()
            } else {
                theme.nonactive_ui_text_color()
            };

            let name = Text::new_inline(*var_name, appearance.monospace_font_family(), 12.)
                .with_color(theme.active_ui_text_color().into_solid())
                .finish();
            let badge = Text::new_inline(badge_label, appearance.ui_font_family(), 11.)
                .with_color(badge_color.into_solid())
                .finish();
            let help = Text::new_inline(*var_help, appearance.ui_font_family(), 11.)
                .with_color(theme.nonactive_ui_text_color().into_solid())
                .finish();

            let row = Container::new(
                Flex::column()
                    .with_cross_axis_alignment(CrossAxisAlignment::Start)
                    .with_child(
                        Flex::row()
                            .with_cross_axis_alignment(CrossAxisAlignment::Center)
                            .with_child(Container::new(name).with_margin_right(12.).finish())
                            .with_child(badge)
                            .finish(),
                    )
                    .with_child(Container::new(help).with_margin_top(2.).finish())
                    .finish(),
            )
            .with_padding_bottom(10.)
            .finish();

            rows.add_child(row);
        }

        Container::new(
            Flex::column()
                .with_cross_axis_alignment(CrossAxisAlignment::Start)
                .with_child(header)
                .with_child(description)
                .with_child(rows.finish())
                .finish(),
        )
        .with_padding_bottom(20.)
        .finish()
    }
}

#[derive(Default)]
struct WebResearchResourcesWidget {
    web_agent_button: MouseStateHandle,
    deep_research_button: MouseStateHandle,
    anthropic_button: MouseStateHandle,
    openai_button: MouseStateHandle,
    openrouter_button: MouseStateHandle,
}

impl SettingsWidget for WebResearchResourcesWidget {
    type View = WebResearchPageView;

    fn search_terms(&self) -> &str {
        "resources documentation github repos providers anthropic openai openrouter"
    }

    fn render(
        &self,
        _view: &Self::View,
        appearance: &Appearance,
        _app: &AppContext,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let ui_builder = appearance.ui_builder();

        let header = Text::new_inline("Resources", appearance.ui_font_family(), 14.)
            .with_style(Properties::default().weight(Weight::Bold))
            .with_color(theme.active_ui_text_color().into_solid())
            .finish();

        let mk = |label: &str, mouse_state: MouseStateHandle, url: &'static str| {
            ui_builder
                .button(ButtonVariant::Outlined, mouse_state)
                .with_text_label(label.to_owned())
                .with_style(UiComponentStyles::default().set_padding(Coords {
                    top: 6.,
                    bottom: 6.,
                    left: 12.,
                    right: 12.,
                }))
                .build()
                .on_click(move |ctx, _, _| {
                    ctx.dispatch_typed_action(WebResearchPageAction::OpenUrl(url.to_string()));
                })
                .finish()
        };

        let buttons = Wrap::row()
            .with_main_axis_size(MainAxisSize::Max)
            .with_children([
                Container::new(mk(
                    "web-agent (firecrawl)",
                    self.web_agent_button.clone(),
                    WEB_AGENT_REPO_URL,
                ))
                .with_margin_right(8.)
                .with_margin_bottom(8.)
                .finish(),
                Container::new(mk(
                    "open-deep-research",
                    self.deep_research_button.clone(),
                    DEEP_RESEARCH_REPO_URL,
                ))
                .with_margin_right(8.)
                .with_margin_bottom(8.)
                .finish(),
                Container::new(mk(
                    "Anthropic keys",
                    self.anthropic_button.clone(),
                    ANTHROPIC_KEYS_URL,
                ))
                .with_margin_right(8.)
                .with_margin_bottom(8.)
                .finish(),
                Container::new(mk(
                    "OpenAI keys",
                    self.openai_button.clone(),
                    OPENAI_KEYS_URL,
                ))
                .with_margin_right(8.)
                .with_margin_bottom(8.)
                .finish(),
                Container::new(mk(
                    "OpenRouter keys",
                    self.openrouter_button.clone(),
                    OPENROUTER_KEYS_URL,
                ))
                .with_margin_bottom(8.)
                .finish(),
            ])
            .finish();

        Container::new(
            Flex::column()
                .with_cross_axis_alignment(CrossAxisAlignment::Start)
                .with_child(Container::new(header).with_margin_bottom(8.).finish())
                .with_child(buttons)
                .finish(),
        )
        .with_padding_bottom(20.)
        .finish()
    }
}
