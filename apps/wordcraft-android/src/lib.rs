//! WordCraft for Android: the desktop app's wiring (runtime, engine, UI) started by NativeActivity.
//!
//! First version: no file dialogs, no system clipboard, no printing, no updater. It opens the
//! sample document, so the window, the ribbon and a physical keyboard (Samsung DeX) can be tested.
#![cfg(target_os = "android")]

use std::sync::{Arc, Mutex};

use craft::host::Host;
use craft::host::target::ShellInfo;
use craft::ui::InstallSpec;
use craft::ui::runtime::windows::{WindowAction, WindowHost};
use craft::ui::shell::embed;
use craft::window::restore::Store;
use craft::window::system::SystemWindows;
use wordcraft_engine::Session;
use wordcraft_ui_egui::{Services, WordApp};

struct App {
    app: WordApp,
}

impl eframe::App for App {
    fn raw_input_hook(&mut self, ctx: &egui::Context, _raw: &mut egui::RawInput) {
        embed::before_input(ctx, |out| self.app.frame_input(out));
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.logic(ctx);
        if self.app.quit_requested && !self.app.printing.busy() {
            WindowHost::of(ctx).act(egui::ViewportId::ROOT, WindowAction::Close);
        }
        embed::logic(ctx, || self.app.close_decision());
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        embed::before_ui(ui.ctx(), frame);
        self.app.ui(ui);
    }

    fn on_exit(&mut self) {
        self.app.session.finish_recovery();
    }
}

#[unsafe(no_mangle)]
fn android_main(android_app: winit::platform::android::activity::AndroidApp) {
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("WordCraft"));
    log::info!("WordCraft {} starting on Android", env!("CARGO_PKG_VERSION"));

    let options = eframe::NativeOptions { android_app: Some(android_app), ..Default::default() };

    let result = eframe::run_native(
        "WordCraft",
        options,
        Box::new(|cc| {
            let ctx = cc.egui_ctx.clone();
            let lang = wordcraft_ui_egui::i18n::Lang::EN;
            let host = Arc::new(Host::builder(wordcraft_ui_egui::APP).i18n(wordcraft_ui_egui::i18n::catalogs()).lang(lang.craft_lang()).build());
            let spec = InstallSpec::new(ShellInfo::current(), Arc::new(SystemWindows))
                .app_id("ai.storyteller.wordcraft")
                .placement(Arc::new(Mutex::new(Store::in_memory())));
            embed::install(&ctx, spec, &wordcraft_ui_egui::ui_fonts(lang)).map_err(|e| format!("the runtime could not be installed: {e}"))?;
            let session = Session::new(wordcraft_engine::sample::sample_document());
            let app = WordApp::new(session, Services::default()).with_host(host);
            Ok(Box::new(App { app }))
        }),
    );
    if let Err(e) = result {
        log::error!("WordCraft stopped: {e}");
    }
}
