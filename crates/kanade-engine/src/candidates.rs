use super::*;

#[derive(Clone)]
pub(super) struct Choice {
    pub value: String,
    pub reading: String,
    pub annotation: String,
    pub source: CandidateSource,
    history_deletable: bool,
    whole_reading: bool,
    transliteration: bool,
}
impl Choice {
    pub(super) fn model_sentence(value: String, reading: String) -> Self {
        Self {
            value,
            reading,
            source: CandidateSource::Llm,
            annotation: "文脈".into(),
            history_deletable: false,
            whole_reading: true,
            transliteration: false,
        }
    }
}
pub(super) fn remember_selection(session: &mut Session, choice: &Choice) {
    if session.privacy != Privacy::Normal
        || choice.source != CandidateSource::Mozc
        || choice.transliteration
    {
        return;
    }
    session
        .selection_intents
        .retain(|word| word.reading != choice.reading);
    if session.selection_intents.len() < 32 {
        session.selection_intents.push(Word {
            reading: choice.reading.clone(),
            value: choice.value.clone(),
        });
    }
}
impl<C: ConversionEngine> Engine<C> {
    pub(super) fn refresh_choices(
        &self,
        session: &mut Session,
        snapshot: &Snapshot,
        app: &str,
    ) -> Option<String> {
        if !snapshot.converting {
            session.choices.clear();
            session.date_candidates = None;
            session.selected_choice = 0;
            session.frozen = None;
            return None;
        }
        let mut preferred = None;
        let focused = snapshot.preedit.segments.get(snapshot.focused_segment);
        let selected = focused.map(|s| s.value.as_str()).unwrap_or("");
        let mut surfaces = std::collections::BTreeSet::new();
        let mut choices: Vec<_> = snapshot
            .candidates
            .iter()
            .filter(|c| surfaces.insert(c.value.as_str()))
            .map(|c| Choice {
                value: c.value.clone(),
                reading: c.reading.clone(),
                annotation: c.annotation.clone(),
                source: CandidateSource::Mozc,
                history_deletable: c.history_deletable,
                whole_reading: false,
                transliteration: c.transliteration,
            })
            .collect();
        if self.settings.learning
            && session.privacy == Privacy::Normal
            && let Some(segment) = focused
        {
            let terms = self
                .learning
                .as_ref()
                .and_then(|l| l.read(|s| s.vocabulary(&segment.reading)))
                .unwrap_or_default();
            for value in terms.into_iter().take(16).rev() {
                if !choices.iter().any(|c| c.value == value) {
                    choices.insert(
                        choices.len().min(1),
                        Choice {
                            value,
                            reading: segment.reading.clone(),
                            annotation: "学習した語".into(),
                            source: CandidateSource::Learned,
                            history_deletable: false,
                            whole_reading: false,
                            transliteration: false,
                        },
                    );
                }
            }
        }
        if self.settings.project_dictionary
            && let Some(segment) = focused
        {
            let whole = reading(&snapshot.preedit);
            let mut values = self.projects.lookup(app, &whole);
            let whole_reading = !values.is_empty();
            let choice_reading = if whole_reading {
                whole
            } else {
                values = self.projects.lookup(app, &segment.reading);
                segment.reading.clone()
            };
            for value in values.into_iter().rev() {
                if !choices.iter().any(|c| c.value == value) {
                    choices.insert(
                        choices.len().min(1),
                        Choice {
                            value,
                            reading: choice_reading.clone(),
                            annotation: "プロジェクトの語".into(),
                            source: CandidateSource::Project,
                            history_deletable: false,
                            whole_reading,
                            transliteration: false,
                        },
                    );
                }
            }
        }
        if self.settings.calculated_dates {
            let reading = reading(&snapshot.preedit);
            let date = session
                .date_candidates
                .as_ref()
                .filter(|(old, _)| old == &reading)
                .map(|(_, date)| *date)
                .or_else(|| self.date.as_ref().and_then(dates::Calendar::current));
            let values = date
                .map(|date| date.candidates(&reading))
                .unwrap_or_default();
            session.date_candidates = date
                .filter(|_| !values.is_empty())
                .map(|date| (reading.clone(), date));
            for value in values.into_iter().rev() {
                if !choices.iter().any(|c| c.value == value) {
                    choices.insert(
                        choices.len().min(1),
                        Choice {
                            value,
                            reading: reading.clone(),
                            annotation: "日付".into(),
                            source: CandidateSource::Date,
                            history_deletable: false,
                            whole_reading: true,
                            transliteration: false,
                        },
                    );
                }
            }
        } else {
            session.date_candidates = None;
        }
        if self.settings.emoji_dictionary {
            let whole = reading(&snapshot.preedit);
            let surface: String = snapshot
                .preedit
                .segments
                .iter()
                .map(|s| s.value.as_str())
                .collect();
            let mut values = self.emoji.lookup(&whole);
            values.extend(self.emoji.lookup(&surface));
            let whole_reading = !values.is_empty();
            if !whole_reading && let Some(segment) = focused {
                // Very short segments are too ambiguous for a phrase fallback.
                if segment.reading.chars().count() >= 3 {
                    values.extend(self.emoji.lookup(&segment.reading));
                    values.extend(self.emoji.lookup(&segment.value));
                }
            }
            let choice_reading = if whole_reading {
                whole
            } else {
                focused.map(|s| s.reading.clone()).unwrap_or_default()
            };
            let mut seen = std::collections::BTreeSet::new();
            let mut extra = Vec::new();
            let positions: std::collections::BTreeMap<_, _> = choices
                .iter()
                .enumerate()
                .map(|(i, c)| (kanade_emoji::identity(&c.value), i))
                .collect();
            for value in values {
                let identity = kanade_emoji::identity(value);
                if seen.contains(&identity) {
                    continue;
                }
                if seen.len() == kanade_emoji::LIMIT {
                    break;
                }
                seen.insert(identity.clone());
                let choice = Choice {
                    value: value.into(),
                    reading: choice_reading.clone(),
                    annotation: String::new(),
                    source: CandidateSource::Emoji,
                    history_deletable: false,
                    whole_reading,
                    transliteration: false,
                };
                if let Some(&position) = positions.get(&identity) {
                    // Resolve VS variants through this catalogue rather than asking
                    // Mozc to select a different byte sequence it does not own.
                    choices[position] = choice;
                } else {
                    extra.push(choice);
                }
            }
            let mut retained = std::collections::BTreeSet::new();
            choices.retain(|c| {
                let identity = kanade_emoji::identity(&c.value);
                !seen.contains(&identity)
                    || (c.source == CandidateSource::Emoji && retained.insert(identity))
            });
            let position = choices.len().min(6);
            choices.splice(position..position, extra);
        }
        // Keep an applied background order while the same list stays on screen.
        let restored = session
            .frozen
            .as_ref()
            .is_some_and(|frozen| frozen.restore(snapshot, &mut choices));
        if !restored {
            session.frozen = None;
            if self.settings.learning
                && session.privacy == Privacy::Normal
                && let Some(segment) = focused
            {
                let preferences = self
                    .learning
                    .as_ref()
                    .and_then(|l| l.read(|s| s.preferences(&segment.reading)))
                    .unwrap_or_default();
                if !preferences.is_empty() {
                    preferred = preferences
                        .iter()
                        .find(|value| {
                            choices.iter().any(|c| {
                                c.source == CandidateSource::Mozc
                                    && !c.transliteration
                                    && c.value == **value
                            })
                        })
                        .cloned();
                    let slots: Vec<_> = choices
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| c.source == CandidateSource::Mozc && !c.transliteration)
                        .map(|(i, _)| i)
                        .collect();
                    let mut ranked: Vec<_> = slots.iter().map(|i| choices[*i].clone()).collect();
                    ranked.sort_by_key(|c| {
                        preferences
                            .iter()
                            .position(|v| *v == c.value)
                            .unwrap_or(usize::MAX)
                    });
                    for (slot, choice) in slots.into_iter().zip(ranked) {
                        choices[slot] = choice;
                    }
                }
                session.frozen = super::refine::Frozen::capture(snapshot, &choices);
            }
        }
        session.selected_choice = choices
            .iter()
            .position(|c| {
                c.value == selected
                    || (c.source == CandidateSource::Emoji
                        && kanade_emoji::identity(&c.value) == kanade_emoji::identity(selected))
            })
            .unwrap_or(0);
        session.choices = choices;
        preferred
    }
    pub(super) fn choose(
        &mut self,
        session: &mut Session,
        choice: &Choice,
        submit: bool,
    ) -> Result<Snapshot, Code> {
        if choice.source == CandidateSource::Emoji && !self.settings.emoji_dictionary {
            return Err(Code::Stale);
        }
        if choice.source == CandidateSource::Project
            && (!self.settings.project_dictionary
                || !self
                    .projects
                    .lookup(&session.app, &choice.reading)
                    .contains(&choice.value))
        {
            return Err(Code::Stale);
        }
        if choice.source == CandidateSource::Mozc {
            let selected = self
                .backend
                .select(session.id, &choice.value)
                .map_err(|_| Code::Unavailable)?;
            session.preview_base = None;
            return if submit {
                self.backend
                    .submit(session.id)
                    .map_err(|_| Code::Unavailable)
            } else {
                Ok(selected)
            };
        }
        let base = session.preview_base.as_ref().unwrap_or(&session.snapshot);
        let mut preview = base.clone();
        let transient = matches!(
            choice.source,
            CandidateSource::Date | CandidateSource::Emoji | CandidateSource::Llm
        );
        if choice.source == CandidateSource::Date && !self.settings.calculated_dates {
            return Err(Code::Stale);
        }
        if choice.whole_reading {
            if reading(&preview.preedit) != choice.reading {
                return Err(Code::Stale);
            }
            // Synthetic readings can span several Mozc segments. Replace exactly
            // the whole displayed reading with the selected value.
            preview.preedit.segments = vec![Segment {
                reading: choice.reading.clone(),
                value: choice.value.clone(),
                focused: true,
                value_utf16: utf16_len(&choice.value),
            }];
            preview.focused_segment = 0;
        }
        let segment = preview
            .preedit
            .segments
            .get_mut(preview.focused_segment)
            .ok_or(Code::Stale)?;
        if segment.reading != choice.reading {
            return Err(Code::Stale);
        }
        segment.value = choice.value.clone();
        segment.value_utf16 = utf16_len(&segment.value);
        preview.preedit.cursor_utf16 = preview.preedit.segments.iter().map(|s| s.value_utf16).sum();
        preview.consumed = true;
        preview.synthetic_preview = true;
        if !submit && session.preview_base.is_none() {
            session.preview_base = Some(session.snapshot.clone());
        }
        if submit {
            let words: Vec<_> = preview
                .preedit
                .segments
                .iter()
                .map(|s| Word {
                    reading: s.reading.clone(),
                    value: s.value.clone(),
                })
                .collect();
            let commit = Commit {
                reading: words.iter().map(|w| w.reading.as_str()).collect(),
                value: words.iter().map(|w| w.value.as_str()).collect(),
                words,
                synthetic: true,
                transient,
            };
            self.backend
                .reset(session.id)
                .map_err(|_| Code::Unavailable)?;
            session.preview_base = None;
            preview = Snapshot {
                consumed: true,
                commit: Some(commit),
                ..Default::default()
            };
        }
        Ok(preview)
    }
    pub(super) fn candidate_key(
        &mut self,
        session: &mut Session,
        key: &Key,
        preceding: &str,
    ) -> Result<Option<Snapshot>, Code> {
        if !session.snapshot.converting || session.choices.is_empty() {
            return Ok(None);
        }
        if key.modifiers != 0 {
            return if session.snapshot.synthetic_preview
                && !key.text.is_empty()
                && key.modifiers & (MOD_CONTROL | MOD_ALT | MOD_META) == 0
            {
                self.continue_preview(session, key, preceding).map(Some)
            } else {
                Ok(None)
            };
        }
        let count = session.choices.len();
        let page_size = self.settings.page_size as usize;
        let digit = key
            .text
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=page_size).contains(n));
        let (slot, submit) = if let Some(digit) = digit {
            (
                (session.selected_choice / page_size) * page_size + digit - 1,
                true,
            )
        } else {
            match SpecialKey::try_from(key.special).map_err(|_| Code::Invalid)? {
                SpecialKey::Enter => (session.selected_choice, true),
                SpecialKey::Space | SpecialKey::Down => {
                    ((session.selected_choice + 1) % count, false)
                }
                SpecialKey::Up => ((session.selected_choice + count - 1) % count, false),
                _ if !key.text.is_empty() && session.snapshot.synthetic_preview => {
                    return self.continue_preview(session, key, preceding).map(Some);
                }
                _ => return Ok(None),
            }
        };
        let Some(choice) = session.choices.get(slot).cloned() else {
            let mut unchanged = session.snapshot.clone();
            unchanged.consumed = true;
            return Ok(Some(unchanged));
        };
        let result = self.choose(session, &choice, submit)?;
        // Enter accepts the shown default; only navigation/digits express a
        // new preference. A prior navigated choice remains pending until ACK.
        if key.special != SpecialKey::Enter as i32 {
            remember_selection(session, &choice);
            session.automatic = false;
        }
        session.selected_choice = slot;
        Ok(Some(result))
    }
    fn continue_preview(
        &mut self,
        session: &mut Session,
        key: &Key,
        preceding: &str,
    ) -> Result<Snapshot, Code> {
        let choice = session
            .choices
            .get(session.selected_choice)
            .cloned()
            .ok_or(Code::Stale)?;
        let committed = self
            .choose(session, &choice, true)?
            .commit
            .ok_or(Code::Unavailable)?;
        let mut context = String::new();
        if session.privacy == Privacy::Normal {
            context.push_str(preceding);
            context.push_str(&committed.value);
            if context.len() > MAX_CONTEXT {
                let mut start = context.len() - MAX_CONTEXT;
                while !context.is_char_boundary(start) {
                    start += 1;
                }
                context.drain(..start);
            }
        }
        let mut next = self
            .backend
            .key(session.id, key, &context)
            .map_err(|_| Code::Unavailable)?;
        next.commit = Some(append_commit(committed, next.commit.take()));
        session.automatic = false;
        Ok(next)
    }
    pub(super) fn delete_selected_history(
        &mut self,
        session: &mut Session,
        privacy: Privacy,
        passed: bool,
    ) -> Result<Snapshot, Code> {
        let mut unchanged = session.snapshot.clone();
        unchanged.consumed = !passed;
        // Never dispatch the upstream keymap's implicit focused-ID deletion for an
        // invisible, private or synthetic choice, or a passed-through application key.
        if passed || privacy != Privacy::Normal || !session.snapshot.converting {
            return Ok(unchanged);
        }
        let Some(choice) = session
            .choices
            .get(session.selected_choice)
            .filter(|c| c.source == CandidateSource::Mozc && c.history_deletable)
            .cloned()
        else {
            return Ok(unchanged);
        };
        let snapshot = match self
            .backend
            .delete_candidate_history(session.id, &choice.value)
        {
            Ok(snapshot) => snapshot,
            Err(kanade_mozc::Error::StaleCandidate) => return Ok(unchanged),
            Err(_) => return Err(Code::Unavailable),
        };
        // A delayed synthetic-history insertion must not immediately restore this
        // term. Leave unrelated receipts and Kanade correction records intact.
        let forget_pending = |s: &mut Session| {
            if let Some(visible) = s.visible.as_mut()
                && visible.value == choice.value
                && visible.history_reading.as_deref() == Some(choice.reading.as_str())
            {
                visible.history_reading = None;
            }
        };
        for other in self.sessions.values_mut() {
            forget_pending(other);
        }
        forget_pending(session);
        Ok(snapshot)
    }
}

pub(super) fn append_commit(mut prefix: Commit, suffix: Option<Commit>) -> Commit {
    if let Some(suffix) = suffix {
        prefix.reading.push_str(&suffix.reading);
        prefix.value.push_str(&suffix.value);
        prefix.words.extend(suffix.words);
        prefix.transient |= suffix.transient;
    }
    prefix
}
