// What the page does once it is open. Vanilla, inline, and nothing else: this file is read from
// a local file with no network of any kind, so there is no fetching, no module loading and no
// origin to have. See docs/ARCHITECTURE.md D13.
//
// EVENTS is written into the page above this script: one entry per event the timeline shows.

(function () {
  'use strict';

  var detail = document.getElementById('detail');
  var counter = document.getElementById('shown-count');
  var chips = Array.prototype.slice.call(document.querySelectorAll('.ev'));
  var selected = null;

  function hiddenByFilter(chip) {
    var node = document.querySelector('.f-node[value="' + chip.dataset.node + '"]');
    var kind = document.querySelector('.f-kind[value="' + chip.dataset.family + '"]');
    return (node && !node.checked) || (kind && !kind.checked);
  }

  function filter() {
    var visible = 0;
    chips.forEach(function (chip) {
      var hide = hiddenByFilter(chip);
      chip.hidden = hide;
      if (!hide) { visible += 1; }
    });
    if (counter) { counter.textContent = String(visible); }
  }

  function field(name, value) {
    var row = document.createElement('div');
    var key = document.createElement('span');
    key.className = 'k';
    key.textContent = name + ': ';
    row.appendChild(key);
    row.appendChild(document.createTextNode(String(value)));
    return row;
  }

  // Built with the document rather than by assigning markup: every string here came out of a
  // guest, and text nodes cannot become elements however the guest spelled them.
  // Chips are written one node's lane at a time, and EVENTS is in the order the run recorded
  // them. With one node the two orders coincide, which is why every stored bundle here agreed;
  // with a cluster -- what this tool is for -- a chip's position in the document is somebody
  // else's event. Each chip carries the slot it belongs to, and this is where it is read.
  function slotOf(chip) { return Number(chip.dataset.index); }

  function chipForSlot(slot) {
    for (var at = 0; at < chips.length; at += 1) {
      if (slotOf(chips[at]) === slot) { return chips[at]; }
    }
    return null;
  }

  function select(slot) {
    var event = EVENTS[slot];
    if (!event) { return; }
    if (selected) { selected.removeAttribute('data-selected'); }
    selected = chipForSlot(slot);
    if (selected) { selected.setAttribute('data-selected', '1'); }

    detail.textContent = '';
    var head = document.createElement('div');
    head.className = 'mono';
    head.textContent = '#' + event.id + '  t' + event.tick + '  node ' + event.node +
      '  task ' + event.task;
    detail.appendChild(head);

    var what = document.createElement('p');
    what.textContent = event.said;
    detail.appendChild(what);

    if (event.draws && event.draws.length) {
      var draws = document.createElement('div');
      draws.className = 'mono';
      event.draws.forEach(function (draw) {
        draws.appendChild(field('decided', draw));
      });
      detail.appendChild(draws);
    }
  }

  function jumpTo(slot) {
    var chip = chipForSlot(slot);
    if (!chip) { return; }
    select(slot);
    chip.scrollIntoView({ block: 'nearest', inline: 'center' });
    chip.focus();
  }

  // Along the timeline rather than along the document: the arrow keys move to the next event, and
  // the next event of a cluster is usually on another node's lane.
  function neighbour(step) {
    var ordered = chips.slice().sort(function (a, b) { return slotOf(a) - slotOf(b); });
    var from = -1;
    if (selected) {
      for (var at = 0; at < ordered.length; at += 1) {
        if (ordered[at] === selected) { from = at; }
      }
    }
    for (var next = from + step; next >= 0 && next < ordered.length; next += step) {
      if (!ordered[next].hidden) { return slotOf(ordered[next]); }
    }
    return -1;
  }

  chips.forEach(function (chip) {
    chip.addEventListener('click', function () { select(slotOf(chip)); });
  });

  Array.prototype.forEach.call(document.querySelectorAll('.f-node, .f-kind'), function (box) {
    box.addEventListener('change', filter);
  });

  Array.prototype.forEach.call(document.querySelectorAll('[data-jump]'), function (button) {
    button.addEventListener('click', function () {
      jumpTo(Number(button.dataset.jump));
    });
  });

  document.addEventListener('keydown', function (key) {
    if (key.target && key.target.tagName === 'INPUT') { return; }
    if (key.key === 'ArrowRight') { jumpTo(neighbour(1)); }
    if (key.key === 'ArrowLeft') { jumpTo(neighbour(-1)); }
  });

  // Copying without the clipboard interface, which several browsers refuse to a page opened from
  // a file. Selecting the text and asking the document to copy it works everywhere this page is
  // meant to be opened.
  Array.prototype.forEach.call(document.querySelectorAll('[data-copy]'), function (button) {
    button.addEventListener('click', function () {
      var source = document.getElementById(button.dataset.copy);
      if (!source) { return; }
      var range = document.createRange();
      range.selectNodeContents(source);
      var selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      try { document.execCommand('copy'); } catch (ignored) { /* left selected to copy by hand */ }
      button.textContent = 'copied';
      window.setTimeout(function () { button.textContent = 'copy'; }, 1500);
    });
  });

  filter();
}());
