// made by CasualArclamp
//! The ListGroup class (ISO/IEC 13522-5 §37): a token group whose items are
//! a list the application edits (AddItem, DelItem), shown a window of them
//! at a time in its cells (Positions), with a selection.
//!
//! After MythTV's freemheg `TokenGroup.cpp` (`MHListGroup`; GPL-2.0-or-later),
//! rewritten. Freemheg reads MultipleSelection from the WrapAround tag (a
//! slip); here it has its own.

use std::rc::Rc;

use crate::asn1::Node;
use crate::engine::{Engine, Files, ev};
use crate::obj::*;
use crate::tags::*;

impl Engine {
    /// The list of ListGroup `t`, to read and change.
    fn list_mut(&mut self, t: &Ref) -> Option<&mut List> {
        match &mut self.objs.get_mut(t)?.kind {
            Kind::TokenGroup(tg) => tg.list.as_deref_mut(),
            _ => None,
        }
    }

    fn list(&self, t: &Ref) -> Option<&List> {
        match &self.objs.get(t)?.kind {
            Kind::TokenGroup(tg) => tg.list.as_deref(),
            _ => None,
        }
    }

    /// A visible back at its original position (where a list cell moved it).
    fn reset_position(&mut self, r: &Ref) {
        let Some(o) = self.objs.get_mut(r) else {
            return;
        };
        let orig = o
            .def
            .get(C_ORIGINAL_POSITION)
            .and_then(|n| Some((n.arg(0)?.int()?, n.arg(1)?.int()?)));
        if let (Kind::Visible(v), Some(p)) = (&mut o.kind, orig) {
            v.pos = p;
            self.dirty = true;
        }
    }

    /// Show the items that fall in a cell (moved to it, running) and stop
    /// the rest; the presented and head/tail events (`MHListGroup::Update`).
    pub(crate) fn list_update(&mut self, t: &Ref, files: &dyn Files) {
        let Some(l) = self.list(t) else { return };
        let (items, first, cells) = (
            l.items.iter().map(|i| i.0.clone()).collect::<Vec<_>>(),
            l.first,
            l.positions.clone(),
        );
        let (mut first_shown, mut last_shown) = (l.first_shown, l.last_shown);
        let mut events = Vec::new();
        if items.is_empty() {
            if first_shown {
                first_shown = false;
                events.push((ev::FIRST_ITEM_PRESENTED, Value::Bool(false)));
            }
            if last_shown {
                last_shown = false;
                events.push((ev::LAST_ITEM_PRESENTED, Value::Bool(false)));
            }
        }
        let last = items.len().saturating_sub(1);
        for (i, item) in items.iter().enumerate() {
            // The cell this item maps onto (0-based).
            let cell = i as i32 + 1 - first;
            if cell >= 0 && (cell as usize) < cells.len() {
                if i == 0 && !first_shown {
                    first_shown = true;
                    events.push((ev::FIRST_ITEM_PRESENTED, Value::Bool(true)));
                }
                if i == last && !last_shown {
                    last_shown = true;
                    events.push((ev::LAST_ITEM_PRESENTED, Value::Bool(true)));
                }
                let p = cells[cell as usize];
                if let Some(Obj {
                    kind: Kind::Visible(v),
                    ..
                }) = self.objs.get_mut(item)
                {
                    v.pos = p;
                    self.dirty = true;
                }
                if self.objs.get(item).is_some_and(|o| !o.running) {
                    self.activation(item, files);
                }
            } else {
                if i == 0 && first_shown {
                    first_shown = false;
                    events.push((ev::FIRST_ITEM_PRESENTED, Value::Bool(false)));
                }
                if i == last && last_shown {
                    last_shown = false;
                    events.push((ev::LAST_ITEM_PRESENTED, Value::Bool(false)));
                }
                if self.objs.get(item).is_some_and(|o| o.running) {
                    self.deactivation(item, files);
                    self.reset_position(item);
                }
            }
        }
        let Some(l) = self.list_mut(t) else { return };
        l.first_shown = first_shown;
        l.last_shown = last_shown;
        let count = items.len() as i32;
        // HeadItems and TailItems: freemheg's reading of the corrigendum.
        if l.last_first != first {
            events.push((ev::HEAD_ITEMS, Value::Int(first)));
        }
        if l.last_count - l.last_first != count - first {
            events.push((ev::TAIL_ITEMS, Value::Int(count - first)));
        }
        l.last_count = count;
        l.last_first = first;
        for (kind, v) in events {
            self.event(t.clone(), kind, Some(v));
        }
    }

    /// Activation of a list: nothing presented yet, then an update.
    pub(crate) fn list_activated(&mut self, t: &Ref, files: &dyn Files) {
        if let Some(l) = self.list_mut(t) {
            l.first_shown = false;
            l.last_shown = false;
            self.list_update(t, files);
        }
    }

    /// Deactivation of a list stops its items.
    pub(crate) fn list_deactivated(&mut self, t: &Ref, files: &dyn Files) {
        let items: Vec<Ref> = match self.list(t) {
            Some(l) => l.items.iter().map(|i| i.0.clone()).collect(),
            None => return,
        };
        for i in items {
            self.deactivation(&i, files);
        }
    }

    /// An index made to wrap round the list (the MHEG corrigendum).
    fn adjust_index(l: &List, i: i32) -> i32 {
        let n = l.items.len() as i32;
        if n == 0 {
            1
        } else if i > n {
            (i - 1) % n + 1
        } else if i < 0 {
            n - (-i) % n
        } else {
            i
        }
    }

    /// An index into the list, wrapped if the list wraps; None if out of it.
    fn list_index(&self, t: &Ref, i: i32) -> Option<i32> {
        let l = self.list(t)?;
        let i = if l.wrap_around {
            Self::adjust_index(l, i)
        } else {
            i
        };
        (1..=l.items.len() as i32).contains(&i).then_some(i)
    }

    fn select(&mut self, t: &Ref, i: i32, on: bool) {
        let Some(l) = self.list_mut(t) else { return };
        let Some(item) = l.items.get(i as usize - 1) else {
            return;
        };
        if item.1 == on {
            return;
        }
        let mut changed = Vec::new();
        if on && !l.multiple_selection {
            for (n, it) in l.items.iter_mut().enumerate() {
                if it.1 {
                    it.1 = false;
                    changed.push((ev::ITEM_DESELECTED, n as i32 + 1));
                }
            }
        }
        l.items[i as usize - 1].1 = on;
        changed.push((
            if on {
                ev::ITEM_SELECTED
            } else {
                ev::ITEM_DESELECTED
            },
            i,
        ));
        for (kind, n) in changed {
            self.event(t.clone(), kind, Some(Value::Int(n)));
        }
    }

    /// The ListGroup actions on `t`; whether `tag` was one.
    pub(crate) fn list_action(
        &mut self,
        tag: u32,
        t: &Ref,
        args: &[Node],
        g: &Rc<str>,
        files: &dyn Files,
    ) -> bool {
        if self.list(t).is_none() {
            return false;
        }
        let arg = |i: usize| args.get(i);
        let set = |e: &mut Engine, i: usize, v: Value| {
            if let Some(r) = arg(i).and_then(|n| object_ref(n, g)) {
                e.set_var(&r, v);
            }
        };
        match tag {
            C_ADD_ITEM => {
                let index = self.gint(arg(1), g);
                let Some(item) = arg(2).and_then(|n| self.gref(n, g)) else {
                    return true;
                };
                let Some(l) = self.list_mut(t) else {
                    return true;
                };
                if l.items.iter().any(|i| i.0 == item)
                    || index < 1
                    || index > l.items.len() as i32 + 1
                {
                    return true;
                }
                l.items.insert(index as usize - 1, (item, false));
                if index <= l.first && l.first < l.items.len() as i32 {
                    l.first += 1;
                }
                self.list_update(t, files);
            }
            C_DEL_ITEM => {
                let Some(item) = arg(1).and_then(|n| self.gref(n, g)) else {
                    return true;
                };
                let Some(l) = self.list_mut(t) else {
                    return true;
                };
                if let Some(i) = l.items.iter().position(|x| x.0 == item) {
                    l.items.remove(i);
                    if (i as i32) + 1 < l.first && l.first > 1 {
                        l.first -= 1;
                    }
                    self.reset_position(&item);
                }
            }
            C_GET_LIST_SIZE => {
                let n = self.list(t).map_or(0, |l| l.items.len() as i32);
                set(self, 1, Value::Int(n));
            }
            C_GET_FIRST_ITEM => {
                let n = self.list(t).map_or(1, |l| l.first);
                set(self, 1, Value::Int(n));
            }
            C_GET_LIST_ITEM | C_GET_ITEM_STATUS => {
                let cell = self.gint(arg(1), g);
                let Some(i) = self.list_index(t, cell) else {
                    return true;
                };
                let Some(item) = self
                    .list(t)
                    .and_then(|l| l.items.get(i as usize - 1))
                    .cloned()
                else {
                    return true;
                };
                let v = if tag == C_GET_LIST_ITEM {
                    Value::ObjRef(item.0)
                } else {
                    Value::Bool(item.1)
                };
                set(self, 2, v);
            }
            C_GET_CELL_ITEM => {
                let Some(l) = self.list(t) else { return true };
                let cell = self
                    .gint(arg(1), g)
                    .clamp(1, l.positions.len().max(1) as i32);
                let at = cell + l.first - 2;
                let v = match l.items.get(usize::try_from(at).unwrap_or(usize::MAX)) {
                    Some(item) => item.0.clone(),
                    // The null reference.
                    None => Ref {
                        group: "".into(),
                        num: 0,
                    },
                };
                set(self, 2, Value::ObjRef(v));
            }
            C_SELECT_ITEM | C_DESELECT_ITEM | C_TOGGLE_ITEM => {
                let cell = self.gint(arg(1), g);
                let Some(i) = self.list_index(t, cell) else {
                    return true;
                };
                let on = match tag {
                    C_SELECT_ITEM => true,
                    C_DESELECT_ITEM => false,
                    _ => !self
                        .list(t)
                        .and_then(|l| l.items.get(i as usize - 1))
                        .is_some_and(|x| x.1),
                };
                self.select(t, i, on);
            }
            C_SCROLL_ITEMS | C_SET_FIRST_ITEM => {
                let mut cell = self.gint(arg(1), g);
                if tag == C_SCROLL_ITEMS {
                    cell += self.list(t).map_or(1, |l| l.first);
                }
                let Some(i) = self.list_index(t, cell) else {
                    return true;
                };
                if let Some(l) = self.list_mut(t) {
                    l.first = i;
                }
                self.list_update(t, files);
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_indices() {
        let mut l = List::default();
        for n in 1..=3 {
            l.items.push((
                Ref {
                    group: "/s".into(),
                    num: n,
                },
                false,
            ));
        }
        assert_eq!(Engine::adjust_index(&l, 4), 1);
        assert_eq!(Engine::adjust_index(&l, 7), 1);
        assert_eq!(Engine::adjust_index(&l, -1), 2);
        assert_eq!(Engine::adjust_index(&l, 2), 2);
        assert_eq!(Engine::adjust_index(&List::default(), 5), 1);
    }
}
