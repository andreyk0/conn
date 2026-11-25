// The page's script. It sends the write token with every change, reloads the page while nothing is
// being read or typed, copies resume commands and opens local files in a pop-up. It writes text
// with `textContent` only; a file's markup comes from its server-rendered page, as nodes.
(() => {
  'use strict';

  const writeToken = document.body.dataset.writeToken || '';
  const hold = document.getElementById('hold');
  const flashBox = document.getElementById('flash');
  const RELOAD_MS = 10000;

  const flash = (text, bad) => {
    if (!flashBox) return;
    flashBox.textContent = text;
    flashBox.className = bad ? 'flash bad' : 'flash';
    flashBox.hidden = false;
  };

  // A reload throws away what the person is in the middle of: a typed field, a body they
  // expanded, a resume command they are about to copy, an open file, a map node being dragged,
  // or a ticked "hold". The
  // answer is why, or null when nothing would be lost.
  const reading = () => {
    if (hold && hold.checked) return 'hold';
    const active = document.activeElement;
    const typing =
      active &&
      active !== hold &&
      ['INPUT', 'SELECT', 'TEXTAREA'].includes(active.tagName);
    if (typing) return 'typing';
    if (document.querySelector('.more-toggle:checked')) return 'a card is expanded';
    // A focused terminal leaves nothing to copy; only a resume command holds the page.
    if (document.querySelector('.focus-result:not([hidden]):not([data-outcome="focused"])')) {
      return 'resume command shown';
    }
    if (document.querySelector('dialog[open]')) return 'file open';
    if (document.querySelector('.replyable.composing')) return 'reply being written';
    if ('dragging' in document.body.dataset) return 'moving a map node';
    return null;
  };

  // The freshness dot, counted on from the model's time: green while the cards are fresh,
  // yellow once they are older than the server's threshold, red while "hold" is ticked. Its
  // title says how old they are and why the page is not reloading, if it is not.
  const dot = document.getElementById('freshness');
  const showAge = () => {
    if (!dot) return;
    const secs = Math.max(0, Math.round((Date.now() - Number(dot.dataset.generatedAt)) / 1000));
    const why = reading();
    const state = why === 'hold' ? 'held' : secs > Number(dot.dataset.freshSecs) ? 'stale' : 'fresh';
    dot.className = 'dot ' + state;
    const text =
      'as of ' +
      (secs < 60 ? secs + ' s' : Math.floor(secs / 60) + ' min') +
      ' ago' +
      (why === null ? '' : '; refresh paused: ' + why);
    dot.title = text;
    dot.setAttribute('aria-label', text);
  };

  // A file's own page is read at length and does not reload.
  if ('reload' in document.body.dataset) {
    setInterval(() => {
      if (reading() === null) location.reload();
    }, RELOAD_MS);
    setInterval(showAge, 1000);
    // A hidden tab's timers are slowed to about once a minute; coming back reloads at once.
    document.addEventListener('visibilitychange', () => {
      if (document.visibilityState === 'visible' && reading() === null) location.reload();
    });
  }

  // A local file opens in a pop-up holding its page's article; a click with a modifier, or a
  // middle click, opens the page itself as any link does.
  const dialog = document.getElementById('file-dialog');
  const openFile = async (link) => {
    const name = dialog.querySelector('.file-dialog-name');
    const body = dialog.querySelector('.file-dialog-body');
    name.textContent = link.title;
    dialog.querySelector('.file-dialog-open').href = link.href;
    body.replaceChildren();
    body.textContent = 'Loading…';
    if (!dialog.open) dialog.showModal();
    try {
      const response = await fetch(link.href, { credentials: 'same-origin' });
      const text = await response.text();
      if (!response.ok) {
        body.textContent = text;
        return;
      }
      const page = new DOMParser().parseFromString(text, 'text/html');
      const article = page.querySelector('article.file-view');
      if (!article) {
        body.textContent = 'The file page had no article to show.';
        return;
      }
      body.replaceChildren(document.importNode(article, true));
      body.scrollTop = 0;
    } catch (error) {
      body.textContent = 'The request failed: ' + error;
    }
  };

  if (dialog) {
    dialog.querySelector('.file-dialog-close').addEventListener('click', () => dialog.close());
    // A click on the backdrop lands on the dialog itself.
    dialog.addEventListener('click', (event) => {
      if (event.target === dialog) dialog.close();
    });
    document.addEventListener('click', (event) => {
      const link = event.target instanceof Element ? event.target.closest('a.file-link') : null;
      const plain =
        event.button === 0 && !event.ctrlKey && !event.metaKey && !event.shiftKey && !event.altKey;
      if (!link || !plain) return;
      event.preventDefault();
      openFile(link);
    });
  }

  const showFocus = (form, result) => {
    const card = form.closest('.card, .row');
    const box = card && card.querySelector('.focus-result');
    if (!box) {
      flash(result.detail, result.outcome !== 'focused');
      return;
    }
    box.hidden = false;
    box.dataset.outcome = result.outcome;
    box.querySelector('.msg').textContent = result.detail;
    // Only a session with no live terminal is resumed; every other outcome is a running
    // session, which a resume would run a second time.
    box.querySelector('.resume').hidden = result.outcome !== 'no_terminal';
  };

  document.addEventListener('submit', async (event) => {
    const form = event.target;
    if (!(form instanceof HTMLFormElement) || !form.classList.contains('act')) return;
    event.preventDefault();
    try {
      const response = await fetch(form.action, {
        method: 'POST',
        credentials: 'same-origin',
        headers: {
          'X-Conn-Token': writeToken,
          'Content-Type': 'application/x-www-form-urlencoded',
        },
        body: new URLSearchParams(new FormData(form)),
      });
      const text = await response.text();
      if (!response.ok) {
        flash(text, true);
        return;
      }
      if (form.dataset.kind === 'focus') {
        showFocus(form, JSON.parse(text));
      } else {
        location.reload();
      }
    } catch (error) {
      flash('The request failed: ' + error, true);
    }
  });

  document.addEventListener('click', async (event) => {
    const button = event.target instanceof Element ? event.target.closest('button.copy') : null;
    if (!button) return;
    const command = button.parentElement.querySelector('pre');
    try {
      await navigator.clipboard.writeText(command.textContent);
      button.textContent = 'Copied';
    } catch (error) {
      button.textContent = 'Copy failed';
    }
    setTimeout(() => {
      button.textContent = 'Copy';
    }, 1500);
  });

  // A button that carries its own text to copy (a PAM grant's Slack ask), and the line said
  // once it is copied.
  document.addEventListener('click', async (event) => {
    const button = event.target instanceof Element ? event.target.closest('button.copy-text') : null;
    if (!button) return;
    try {
      await navigator.clipboard.writeText(button.dataset.copy);
      flash(button.dataset.copied, false);
    } catch (error) {
      flash('Could not copy: ' + error, true);
    }
  });

  // A follow-up's prompt for a session in a terminal: copied, and the terminal raised through
  // the row's own "Open terminal" form, for the person to paste into.
  document.addEventListener('click', async (event) => {
    const button = event.target instanceof Element ? event.target.closest('button.copy-open') : null;
    if (!button) return;
    const row = button.closest('.followup');
    const prompt = button.parentElement.querySelector('pre');
    try {
      await navigator.clipboard.writeText(prompt.textContent);
    } catch (error) {
      flash('Could not copy: ' + error, true);
      return;
    }
    const focus = row && row.querySelector('form[data-kind="focus"]');
    if (focus) focus.requestSubmit();
    flash('Prompt copied: paste it in the terminal (Ctrl+Shift+V) and press Enter.', false);
  });

  // The card last clicked is outlined, so coming back from its terminal finds it again. It is
  // kept per tab across the page's reloads; storage that is blocked only loses the outline.
  const ACTIVE_KEY = 'conn.inbox.active';
  const markActive = (id) => {
    for (const card of document.querySelectorAll('article.card.active')) {
      card.classList.remove('active');
    }
    const card = id ? document.getElementById(id) : null;
    if (card && card.matches('article.card')) card.classList.add('active');
  };
  try {
    markActive(sessionStorage.getItem(ACTIVE_KEY));
  } catch (error) {
    // No storage: nothing is outlined.
  }
  document.addEventListener('click', (event) => {
    const card = event.target instanceof Element ? event.target.closest('article.card') : null;
    if (!card || !card.id) return;
    markActive(card.id);
    try {
      sessionStorage.setItem(ACTIVE_KEY, card.id);
    } catch (error) {
      // No storage: the outline lasts until the next reload.
    }
  });

  // --- the reply composer ---
  //
  // A turn's text is answered in a mode the person switches its card into with Reply: only
  // then does each block of the text get a ↩, and a click on the text itself never starts
  // anything. A ↩ opens a reply under its block. The message quotes each answered block, in
  // the text's order, with its reply under it, then the general text. Sending copies the
  // message and, for a session running in a terminal, raises that terminal through the card's
  // own "Open terminal" form; the person pastes it there and presses Enter.
  //
  // The draft is kept per tab under the session and the instant its turn ended (the
  // `data-reply-key`), so a reload brings it back and the next turn starts clean.
  const QUOTE_CHARS = 160;
  const DRAFT_PREFIX = 'conn.reply.';

  // The blocks a reply can answer: a paragraph not inside a list item (the item answers for
  // it), a list item, a heading, a code block and a table body row.
  const blocksOf = (region) =>
    [...region.querySelectorAll('p, li, h3, h4, pre, tr')].filter(
      (block) =>
        !block.closest('.composer, .reply-slot') &&
        !(block.tagName === 'P' && block.parentElement.closest('li')) &&
        !(block.tagName === 'TR' && block.closest('thead')),
    );

  // The text a reply quotes: the block's own words on one line, an ordered item led by its
  // number, a table row's cells joined, cut at a word.
  // The number of a structured question's option, or null for any other block.
  const optionNumber = (block) => {
    const list = block.parentElement;
    if (block.tagName !== 'LI' || list.tagName !== 'OL' || !block.closest('.question')) return null;
    const items = [...list.children].filter((child) => child.tagName === 'LI');
    return Number(list.getAttribute('start') || 1) + items.indexOf(block);
  };

  const quoteOf = (block) => {
    // A question's option is quoted by its label: the description is the agent's own gloss.
    const option = optionNumber(block);
    const label = block.querySelector(':scope > strong');
    if (option !== null && label) return option + '. ' + label.textContent.trim();
    const copy = block.cloneNode(true);
    for (const inner of copy.querySelectorAll('ul, ol, .reply-mark, .reply-slot')) inner.remove();
    let text =
      block.tagName === 'TR'
        ? [...copy.children].map((cell) => cell.textContent.trim()).join(' | ')
        : copy.textContent;
    text = text.replace(/\s+/g, ' ').trim();
    const list = block.parentElement;
    if (block.tagName === 'LI' && list.tagName === 'OL') {
      const items = [...list.children].filter((child) => child.tagName === 'LI');
      text = Number(list.getAttribute('start') || 1) + items.indexOf(block) + '. ' + text;
    }
    if (text.length <= QUOTE_CHARS) return text;
    const cut = text.slice(0, QUOTE_CHARS);
    const space = cut.lastIndexOf(' ');
    return (space > QUOTE_CHARS / 2 ? cut.slice(0, space) : cut) + '…';
  };

  // A reply's slot goes under its block: inside a list item, after a table, after anything
  // else.
  const slotPlace = (block) => {
    if (block.tagName === 'LI') return { parent: block, before: null };
    const anchor = block.tagName === 'TR' ? block.closest('table') : block;
    return { parent: anchor.parentElement, before: anchor.nextSibling };
  };

  const composer = {
    key: (region) => DRAFT_PREFIX + region.dataset.replyKey,

    save(region) {
      const draft = {
        slots: [...region.querySelectorAll('.reply-slot')].map((slot) => ({
          block: Number(slot.dataset.block),
          text: slot.querySelector('textarea').value,
        })),
        general: region.querySelector('.reply-general').value,
      };
      try {
        sessionStorage.setItem(composer.key(region), JSON.stringify(draft));
      } catch (error) {
        // No storage: the draft lasts as long as the page, which does not reload meanwhile.
      }
    },

    forget(region) {
      try {
        sessionStorage.removeItem(composer.key(region));
      } catch (error) {
        // No storage: nothing was kept.
      }
    },

    enter(region) {
      region.classList.add('composing');
      region.querySelector('.composer-body').hidden = false;
      region.querySelector('.reply-open').hidden = true;
      // A collapsed body is opened, so every block can be answered.
      const toggle = region.querySelector('.more-toggle');
      if (toggle) toggle.checked = true;
      blocksOf(region).forEach((block, index) => {
        const mark = document.createElement('button');
        mark.type = 'button';
        mark.className = 'reply-mark';
        mark.dataset.block = String(index);
        mark.title = 'Reply to this';
        mark.setAttribute('aria-label', 'Reply to this');
        mark.textContent = '↩';
        // A list item's ↩ ends its own words, before any list nested in it; a table row's sits
        // in its last cell.
        if (block.tagName === 'TR') {
          if (block.lastElementChild) block.lastElementChild.append(mark);
        } else {
          const nested = [...block.children].find((child) => ['UL', 'OL'].includes(child.tagName));
          block.insertBefore(mark, nested || null);
        }
      });
    },

    leave(region) {
      region.classList.remove('composing');
      for (const node of region.querySelectorAll('.reply-mark, .reply-slot')) node.remove();
      region.querySelector('.reply-general').value = '';
      region.querySelector('.composer-body').hidden = true;
      region.querySelector('.reply-open').hidden = false;
    },

    // The slot answering block `index`, opened if it is not; focused either way.
    open(region, index, text) {
      const existing = region.querySelector(`.reply-slot[data-block="${index}"]`);
      if (existing) {
        existing.querySelector('textarea').focus();
        return;
      }
      const block = blocksOf(region)[index];
      if (!block) return;
      const slot = document.createElement('div');
      slot.className = 'reply-slot';
      slot.dataset.block = String(index);
      const quote = document.createElement('blockquote');
      quote.textContent = quoteOf(block);
      const area = document.createElement('textarea');
      area.rows = 2;
      area.placeholder = 'Your reply; left empty, it picks this one';
      area.value = text || '';
      const remove = document.createElement('button');
      remove.type = 'button';
      remove.className = 'reply-remove';
      remove.title = 'Remove this reply';
      remove.textContent = '×';
      slot.append(quote, area, remove);
      const place = slotPlace(block);
      place.parent.insertBefore(slot, place.before);
      area.focus();
    },

    // The options of a question picked with nothing written anywhere: those are answered by
    // their keys in the terminal's dialog, since a paste there becomes a free-text answer.
    // Null when anything was written, or a block that is not an option was answered.
    picksOnly(region) {
      if (region.dataset.answers !== 'question') return null;
      if (region.querySelector('.reply-general').value.trim() !== '') return null;
      const blocks = blocksOf(region);
      const picks = [];
      for (const slot of region.querySelectorAll('.reply-slot')) {
        const option = optionNumber(blocks[Number(slot.dataset.block)]);
        if (option === null || slot.querySelector('textarea').value.trim() !== '') return null;
        picks.push(option);
      }
      return picks.length > 0 ? picks.sort((a, b) => a - b) : null;
    },

    // The message: each answered block quoted, in the text's order, its reply under it; then
    // the general text. Empty when nothing was written or picked.
    message(region) {
      const blocks = blocksOf(region);
      const parts = [...region.querySelectorAll('.reply-slot')]
        .map((slot) => ({ index: Number(slot.dataset.block), slot }))
        .sort((a, b) => a.index - b.index)
        .map(({ index, slot }) => {
          const reply = slot.querySelector('textarea').value.trim();
          return '> ' + quoteOf(blocks[index]) + '\n' + (reply === '' ? 'This one.' : reply);
        });
      const general = region.querySelector('.reply-general').value.trim();
      if (general !== '') parts.push(general);
      return parts.join('\n\n');
    },

    restore(region) {
      let draft = null;
      try {
        draft = JSON.parse(sessionStorage.getItem(composer.key(region)) || 'null');
      } catch (error) {
        return;
      }
      if (!draft) return;
      composer.enter(region);
      for (const slot of draft.slots || []) composer.open(region, slot.block, slot.text);
      region.querySelector('.reply-general').value = draft.general || '';
      if (document.activeElement instanceof HTMLElement) document.activeElement.blur();
    },
  };

  for (const region of document.querySelectorAll('.replyable')) composer.restore(region);

  document.addEventListener('click', async (event) => {
    const target = event.target instanceof Element ? event.target : null;
    const region = target && target.closest('.replyable');
    if (!region) return;
    if (target.closest('.reply-open')) {
      composer.enter(region);
      composer.save(region);
      region.querySelector('.reply-general').focus();
    } else if (target.closest('.reply-mark')) {
      composer.open(region, Number(target.closest('.reply-mark').dataset.block), '');
      composer.save(region);
    } else if (target.closest('.reply-remove')) {
      target.closest('.reply-slot').remove();
      composer.save(region);
    } else if (target.closest('.reply-discard')) {
      composer.leave(region);
      composer.forget(region);
    } else if (target.closest('.reply-send')) {
      const note = region.querySelector('.reply-note');
      const terminal = target.closest('.reply-send').dataset.delivery === 'terminal';
      const raise = () => {
        const scope = region.closest('article.card') || document;
        const focus = scope.querySelector('form[data-kind="focus"]');
        if (focus) focus.requestSubmit();
      };
      // A bare pick of a question's options is the dialog's own choice, not text to paste.
      const picks = terminal ? composer.picksOnly(region) : null;
      if (picks !== null) {
        composer.leave(region);
        composer.forget(region);
        raise();
        flash('Nothing copied: in the terminal\'s question, choose ' + picks.join(' and ') + '.', false);
        return;
      }
      const message = composer.message(region);
      if (message === '') {
        note.textContent = 'Nothing to send: write a reply or pick a line.';
        return;
      }
      try {
        await navigator.clipboard.writeText(message);
      } catch (error) {
        note.textContent = 'Could not copy: ' + error;
        return;
      }
      composer.leave(region);
      composer.forget(region);
      if (terminal) {
        raise();
        flash(
          region.dataset.answers === 'question'
            ? 'Reply copied: paste it in the terminal (Ctrl+Shift+V), where it becomes the question\'s typed answer, and press Enter.'
            : 'Reply copied: paste it in the terminal (Ctrl+Shift+V) and press Enter.',
          false,
        );
      } else {
        flash('Reply copied: resume the session and paste it.', false);
      }
    }
  });

  document.addEventListener('input', (event) => {
    const region =
      event.target instanceof Element ? event.target.closest('.replyable.composing') : null;
    if (region) composer.save(region);
  });

  // The filter hides the rows whose text lacks what is typed, and each `data-filter-scope`
  // (a table, a section) none of whose rows is left. It is kept in the URL's `filter`
  // parameter, so the page's own reloads keep it, and is applied again on load.
  const filter = document.getElementById('filter');
  if (filter) {
    const apply = () => {
      const needle = filter.value.trim().toLowerCase();
      for (const row of document.querySelectorAll('[data-filter]')) {
        row.hidden = needle !== '' && !row.dataset.filter.includes(needle);
      }
      for (const scope of document.querySelectorAll('[data-filter-scope]')) {
        scope.hidden = !scope.querySelector('[data-filter]:not([hidden])');
      }
    };
    filter.value = new URL(location.href).searchParams.get('filter') ?? '';
    apply();
    filter.addEventListener('input', () => {
      apply();
      const url = new URL(location.href);
      if (filter.value.trim() === '') url.searchParams.delete('filter');
      else url.searchParams.set('filter', filter.value);
      history.replaceState(null, '', url);
    });
  }
})();
