/* application.rs
 *
 * Copyright 2026 sequ
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

use gettextrs::gettext;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::config::VERSION;
use crate::TimetrackWindow;

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct TimetrackApplication {}

    #[glib::object_subclass]
    impl ObjectSubclass for TimetrackApplication {
        const NAME: &'static str = "TimetrackApplication";
        type Type = super::TimetrackApplication;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for TimetrackApplication {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_gactions();
            obj.set_accels_for_action("app.quit", &["<control>q"]);
        }
    }

    impl ApplicationImpl for TimetrackApplication {
        // We connect to the activate callback to create a window when the application
        // has been launched. Additionally, this callback notifies us when the user
        // tries to launch a "second instance" of the application. When they try
        // to do that, we'll just present any existing window.
        fn activate(&self) {
            let application = self.obj();
            // Get the current window or create one if necessary
            let window = application.active_window().unwrap_or_else(|| {
                let window = TimetrackWindow::new(&*application);
                window.upcast()
            });

            // Ask the window manager/compositor to present the window
            window.present();
        }
    }

    impl GtkApplicationImpl for TimetrackApplication {}
    impl AdwApplicationImpl for TimetrackApplication {}
}

glib::wrapper! {
    pub struct TimetrackApplication(ObjectSubclass<imp::TimetrackApplication>)
        @extends gio::Application, gtk::Application, adw::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl TimetrackApplication {
    pub fn new(application_id: &str, flags: &gio::ApplicationFlags) -> Self {
        glib::Object::builder()
            .property("application-id", application_id)
            .property("flags", flags)
            .property("resource-base-path", "/org/sequ/timetrack")
            .build()
    }

    fn setup_gactions(&self) {
        let quit_action = gio::ActionEntry::builder("quit")
            .activate(move |app: &Self, _, _| app.quit())
            .build();
        let about_action = gio::ActionEntry::builder("about")
            .activate(move |app: &Self, _, _| app.show_about())
            .build();
        let shortcuts_action = gio::ActionEntry::builder("shortcuts")
            .activate(move |app: &Self, _, _| app.show_shortcuts())
            .build();
        let preferences_action = gio::ActionEntry::builder("preferences")
            .activate(move |app: &Self, _, _| app.show_preferences())
            .build();
        self.add_action_entries([
            quit_action,
            about_action,
            shortcuts_action,
            preferences_action,
        ]);
        self.set_accels_for_action("app.shortcuts", &["<control>question"]);
        self.set_accels_for_action("app.preferences", &["<control>comma"]);
    }

    fn show_preferences(&self) {
        // There is no preferences UI yet. Register the action anyway so the
        // menu item is not dead: activating it reports the missing feature
        // instead of silently doing nothing.
        if let Some(window) = self.active_window() {
            let dialog = adw::AlertDialog::builder()
                .heading("Preferences")
                .body("Preferences are not implemented yet.")
                .build();
            dialog.present(Some(&window));
        }
    }

    fn show_shortcuts(&self) {
        if let Some(window) = self.active_window() {
            // shortcuts-dialog.ui is bundled as a standalone resource rather
            // than part of the window template, so build it on demand.
            // AdwShortcutsDialog is a template-only class with no typed
            // binding; it derives from AdwDialog, which is what we present.
            let builder = gtk::Builder::from_resource("/org/sequ/timetrack/shortcuts-dialog.ui");
            let dialog = builder
                .object::<adw::Dialog>("shortcuts_dialog")
                .expect("shortcuts_dialog should be defined in shortcuts-dialog.ui");
            dialog.present(Some(&window));
        }
    }

    fn show_about(&self) {
        let window = self.active_window().unwrap();
        let about = adw::AboutDialog::builder()
            .application_name("Timetrack")
            .application_icon("org.sequ.timetrack")
            .developer_name("sequ")
            .version(VERSION)
            .developers(vec!["sequ"])
            // Translators: Replace "translator-credits" with your name/username, and optionally an email or URL.
            .translator_credits(gettext("translator-credits"))
            .copyright("© 2026 sequ")
            .build();

        about.present(Some(&window));
    }
}
