//! Shell navigation behavior.

use super::*;

impl Shell {
    pub(super) fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.route = Route::Settings(section);
        self.transcript_search = None;
        self.nav.push(NavEntry::Settings(section));
        self.sync_changes_watch(cx);
        self.user_menu_open = false;
        self.chat_menu = None;
        cx.notify();
    }

    pub(super) fn open_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.transcript_search = None;
        self.state
            .update(cx, |state, cx| state.select_chat(Some(chat_id), cx));
        cx.notify();
    }

    /// Select a session by its position in the currently filtered sidebar.
    pub(super) fn select_sidebar_session(&mut self, position: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.active_rows(cx).get(position) {
            self.open_chat(row.id.clone(), cx);
        }
    }

    /// Open the new-session page. Every entry point shares the same target
    /// resolver: sidebar filter, last active space, first space.
    pub(super) fn open_new_session(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.transcript_search = None;
        self.nav.push(NavEntry::Chat(String::new()));
        self.user_menu_open = false;
        self.chat_menu = None;
        let target = {
            let state = self.state.read(cx);
            let valid = |id: &String| state.space_row(id).is_some();
            self.settings
                .space_filter
                .clone()
                .filter(valid)
                .or_else(|| self.settings.last_space_id.clone().filter(valid))
                .or_else(|| state.spaces.first().map(|space| space.id.clone()))
        };
        self.state.update(cx, |state, cx| {
            if target.is_some() {
                state.select_space(target, cx);
            }
            state.select_chat(None, cx);
        });
        self.sync_changes_watch(cx);
        cx.notify();
    }

    pub(super) fn close_secondary_page(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.nav.push(NavEntry::Chat(self.active_chat.clone()));
        self.sync_changes_watch(cx);
        cx.notify();
    }

    pub(super) fn dismiss_settings_modal(&mut self, cx: &mut Context<Self>) -> bool {
        let Route::Settings(section) = self.route else {
            return false;
        };
        match section {
            SettingsSection::Devices => self
                .devices_page
                .as_ref()
                .is_some_and(|page| page.update(cx, |page, cx| page.dismiss_modal(cx))),
            SettingsSection::Agents => self
                .accounts_page
                .as_ref()
                .is_some_and(|page| page.update(cx, |page, cx| page.dismiss_modal(cx))),
            SettingsSection::Secrets => self
                .secrets_page
                .as_ref()
                .is_some_and(|page| page.update(cx, |page, cx| page.dismiss_modal(cx))),
            SettingsSection::Appearance => self
                .appearance_page
                .as_ref()
                .is_some_and(|page| page.update(cx, |page, cx| page.dismiss_modal(cx))),
            SettingsSection::Harnesses
            | SettingsSection::VersionControl
            | SettingsSection::Terminal
            | SettingsSection::Notifications
            | SettingsSection::Hotkeys => false,
        }
    }

    // ---- back/forward (route history) ----

    pub(super) fn navigate_back(&mut self, cx: &mut Context<Self>) {
        while let Some(entry) = self.nav.back() {
            if self.nav_entry_available(&entry, cx) {
                self.apply_nav(entry, cx);
                break;
            }
        }
    }

    pub(super) fn navigate_forward(&mut self, cx: &mut Context<Self>) {
        while let Some(entry) = self.nav.forward() {
            if self.nav_entry_available(&entry, cx) {
                self.apply_nav(entry, cx);
                break;
            }
        }
    }

    pub(super) fn nav_entry_available(&self, entry: &NavEntry, cx: &App) -> bool {
        match entry {
            NavEntry::Chat(chat_id) if !chat_id.is_empty() => self
                .state
                .read(cx)
                .chats
                .iter()
                .any(|chat| chat.id == *chat_id),
            NavEntry::Chat(_) | NavEntry::Settings(_) => true,
        }
    }

    /// Land on a history entry WITHOUT recording a new one: the stack already
    /// points at `entry` (back/forward moved the index); the selection change
    /// this triggers dedups against `current()` in [`Self::on_state_changed`].
    pub(super) fn apply_nav(&mut self, entry: NavEntry, cx: &mut Context<Self>) {
        match entry {
            NavEntry::Chat(chat_id) => {
                self.route = Route::Chat;
                let target = (!chat_id.is_empty()).then_some(chat_id);
                if self.state.read(cx).selected_chat != target {
                    self.state.update(cx, |s, cx| s.select_chat(target, cx));
                }
            }
            NavEntry::Settings(section) => {
                self.route = Route::Settings(section);
            }
        }
        self.sync_changes_watch(cx);
        self.user_menu_open = false;
        self.chat_menu = None;
        cx.notify();
    }

    /// Lazily create the entity for a settings section and return it renderable.
    pub(super) fn settings_outlet(
        &mut self,
        section: SettingsSection,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match section {
            SettingsSection::Devices => {
                if self.devices_page.is_none() {
                    let state = self.state.clone();
                    let background_service = self.background_service.clone();
                    self.devices_page =
                        Some(cx.new(|cx| DevicesPage::new(state, background_service, cx)));
                }
                match &self.devices_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Harnesses => {
                if self.harnesses_page.is_none() {
                    let state = self.state.clone();
                    self.harnesses_page = Some(cx.new(|cx| HarnessesPage::new(state, cx)));
                }
                match &self.harnesses_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Agents => {
                if self.accounts_page.is_none() {
                    let state = self.state.clone();
                    self.accounts_page = Some(cx.new(|cx| AccountsPage::new(state, cx)));
                }
                match &self.accounts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Secrets => {
                if self.secrets_page.is_none() {
                    let state = self.state.clone();
                    self.secrets_page = Some(cx.new(|cx| SecretsPage::new(state, cx)));
                }
                match &self.secrets_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::VersionControl => {
                if self.vcs_page.is_none() {
                    let state = self.state.clone();
                    self.vcs_page = Some(cx.new(|cx| VcsPage::new(state, cx)));
                }
                match &self.vcs_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Terminal => {
                if self.terminal_page.is_none() {
                    let state = self.state.clone();
                    let page = cx.new(|cx| TerminalPage::new(state, cx));
                    self.terminal_page = Some(page);
                }
                match &self.terminal_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Appearance => {
                if self.appearance_page.is_none() {
                    self.appearance_page = Some(cx.new(AppearancePage::new));
                }
                match &self.appearance_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Notifications => {
                if self.notifications_page.is_none() {
                    let system_notifications_enabled = self.settings.system_notifications_enabled;
                    let page = cx.new(|_| NotificationsPage::new(system_notifications_enabled));
                    self.notifications_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &NotificationsEvent, cx| {
                            let NotificationsEvent::SystemNotificationsEnabledChanged(enabled) =
                                event;
                            this.settings.system_notifications_enabled = *enabled;
                            crate::toast::configure(this.settings.system_notifications_enabled, cx);
                            this.schedule_save(cx);
                        },
                    ));
                    self.notifications_page = Some(page);
                }
                match &self.notifications_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Hotkeys => {
                if self.hotkeys_page.is_none() {
                    let state = self.state.clone();
                    let keymap = self.settings.keymap.clone();
                    let page = cx.new(|cx| HotkeysPage::new(state, keymap, cx));
                    // Persist + re-apply the keymap whenever the page changes it.
                    self.hotkeys_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &HotkeysEvent, cx| {
                            let HotkeysEvent::Changed(keymap) = event;
                            this.settings.keymap = keymap.clone();
                            apply_keymap(cx, keymap);
                            // gpui snapshots menu key equivalents in `set_menus`.
                            cx.set_menus(crate::app_menus::app_menus());
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.hotkeys_page = Some(page);
                }
                match &self.hotkeys_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
        }
    }
}
