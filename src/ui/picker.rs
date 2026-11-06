//! Dropdown over items with ids. Rows dim an item when told to.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;

use super::{fitted_label, set_accessible_name};

#[derive(Clone, PartialEq)]
pub struct Item<T> {
    pub id: T,
    pub label: String,
    pub dimmed: bool,
}

impl<T> Item<T> {
    pub fn new(id: T, label: impl Into<String>) -> Self {
        Self { id, label: label.into(), dimmed: false }
    }
}

pub struct Picker<T> {
    pub dropdown: gtk::DropDown,
    model: gtk::StringList,
    items: Rc<RefCell<Vec<Item<T>>>>,
    updating: Cell<bool>,
}

impl<T: Clone + PartialEq + 'static> Picker<T> {
    pub fn new(name: &str, on_pick: impl Fn(T) + 'static) -> Rc<Self> {
        let model = gtk::StringList::new(&[]);
        let dropdown = gtk::DropDown::builder().model(&model).hexpand(true).build();
        set_accessible_name(&dropdown, name);

        let items: Rc<RefCell<Vec<Item<T>>>> = Rc::default();
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
                item.set_child(Some(&fitted_label(None)));
            }
        });
        let bound = Rc::clone(&items);
        factory.connect_bind(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
            let Some(label) = item.child().and_downcast::<gtk::Label>() else { return };
            let Some(text) = item.item().and_downcast::<gtk::StringObject>() else { return };
            label.set_text(&text.string());
            let dimmed = bound.borrow().get(item.position() as usize).is_some_and(|i| i.dimmed);
            if dimmed {
                label.add_css_class("dim-label");
            } else {
                label.remove_css_class("dim-label");
            }
        });
        dropdown.set_factory(Some(&factory));

        let picker = Rc::new(Self { dropdown, model, items, updating: Cell::new(false) });
        let weak = Rc::downgrade(&picker);
        picker.dropdown.connect_selected_notify(move |_| {
            let Some(picker) = weak.upgrade() else { return };
            if !picker.updating.get() {
                if let Some(id) = picker.selected() {
                    on_pick(id);
                }
            }
        });
        picker
    }

    /// Rebuilds the list only if it changed. Then selects `selected`.
    pub fn set_items(&self, items: Vec<Item<T>>, selected: Option<&T>) {
        self.updating.set(true);
        if *self.items.borrow() != items {
            *self.items.borrow_mut() = items;
            let items = self.items.borrow();
            let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
            self.model.splice(0, self.model.n_items(), &labels);
        }
        let position = selected
            .and_then(|id| self.items.borrow().iter().position(|i| i.id == *id))
            .map_or(gtk::INVALID_LIST_POSITION, |p| p as u32);
        self.dropdown.set_selected(position);
        self.updating.set(false);
    }

    /// Selects without counting as a user pick.
    pub fn select(&self, id: &T) {
        let items = self.items.borrow().clone();
        self.set_items(items, Some(id));
    }

    pub fn selected(&self) -> Option<T> {
        self.items.borrow().get(self.dropdown.selected() as usize).map(|i| i.id.clone())
    }
}
