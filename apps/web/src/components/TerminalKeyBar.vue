<script setup lang="ts">
import Icon from "../features/Icon.vue";
import { useI18n } from "../i18n";
import type { ArrowKey } from "../features/terminal-keys";
const { t } = useI18n();

/**
 * On-screen keys, for a device whose keyboard has no arrows and cannot be
 * relied on to be visible at all. The pane shows the bar only where the
 * pointer is a fingertip, and does the sending; the bar only says which key.
 */
/** One-shot Ctrl: the next character the soft keyboard types becomes a control code. */
const ctrlArmed = defineModel<boolean>("ctrlArmed", { required: true });
/** One-shot Shift: Tab becomes Shift+Tab, an arrow a Shift+arrow, a letter its capital. */
const shiftArmed = defineModel<boolean>("shiftArmed", { required: true });
/** Tap a row to move a list selection onto it; see the pane for why it is opt-in. */
const tapSelect = defineModel<boolean>("tapSelect", { required: true });
/** The less used keys fold behind ⋯ so the everyday ones fit one phone-wide row. */
const moreKeys = defineModel<boolean>("moreKeys", { required: true });
const emit = defineEmits<{ escape: []; tab: []; slash: []; arrow: [key: ArrowKey]; enter: []; paste: []; pickFiles: []; keyboard: [] }>();

/** Keep the caret in the terminal so tapping a key never closes the soft keyboard. */
function keepFocus(event: Event) { event.preventDefault(); }
</script>

<template>
  <div class="terminal-keys" role="group" :aria-label="t('Terminal keys')">
    <div v-if="moreKeys" class="terminal-keys-row">
      <button type="button" class="key-labeled" :aria-label="t('Paste')" :title="t('Paste')" @click="moreKeys = false; emit('paste')">
        <Icon name="clipboard" :size="16" /><span>{{ t('Paste') }}</span>
      </button>
      <button type="button" class="key-labeled" :aria-label="t('Photo or file')" :title="t('Photo or file')" @click="emit('pickFiles')">
        {{ t('Photo / file') }}
      </button>
      <button type="button" class="key-labeled" :aria-label="t('Keyboard')" :title="t('Keyboard')" @click="moreKeys = false; emit('keyboard')">
        <Icon name="edit" :size="16" /><span>{{ t('Keyboard') }}</span>
      </button>
      <button type="button" :class="['key-labeled', { armed: tapSelect }]" :aria-pressed="tapSelect" :aria-label="t('Tap a row to select it')" :title="t('Tap a row to select it')" @pointerdown="keepFocus" @click="tapSelect = !tapSelect">
        <Icon name="locate" :size="16" /><span>{{ t('Select rows') }}</span>
      </button>
    </div>
    <div class="terminal-keys-row">
      <button type="button" class="key-text" :aria-label="t('Escape')" :title="t('Escape')" @pointerdown="keepFocus" @click="emit('escape')">Esc</button>
      <button type="button" class="key-text" :class="{ armed: ctrlArmed }" :aria-pressed="ctrlArmed" :aria-label="t('Ctrl')" :title="t('Ctrl')" @pointerdown="keepFocus" @click="ctrlArmed = !ctrlArmed">Ctrl</button>
      <button type="button" class="key-text" :class="{ armed: shiftArmed }" :aria-pressed="shiftArmed" :aria-label="t('Shift')" :title="t('Shift')" @pointerdown="keepFocus" @click="shiftArmed = !shiftArmed">⇧</button>
      <button type="button" class="key-text" :aria-label="t('Tab')" :title="t('Tab')" @pointerdown="keepFocus" @click="emit('tab')">Tab</button>
      <button type="button" class="key-text" :aria-label="t('Slash · command menu')" :title="t('Slash · command menu')" @pointerdown="keepFocus" @click="emit('slash')">/</button>
      <span class="terminal-keys-sep" aria-hidden="true" />
      <button type="button" :aria-label="t('Arrow left')" :title="t('Arrow left')" @pointerdown="keepFocus" @click="emit('arrow', 'left')"><Icon name="chevron" :size="16" class="key-left" /></button>
      <button type="button" :aria-label="t('Arrow up')" :title="t('Arrow up')" @pointerdown="keepFocus" @click="emit('arrow', 'up')"><Icon name="chevron" :size="16" class="key-up" /></button>
      <button type="button" :aria-label="t('Arrow down')" :title="t('Arrow down')" @pointerdown="keepFocus" @click="emit('arrow', 'down')"><Icon name="chevron" :size="16" class="key-down" /></button>
      <button type="button" :aria-label="t('Arrow right')" :title="t('Arrow right')" @pointerdown="keepFocus" @click="emit('arrow', 'right')"><Icon name="chevron" :size="16" /></button>
      <button type="button" :aria-label="t('Enter')" :title="t('Enter')" @pointerdown="keepFocus" @click="emit('enter')"><Icon name="check" :size="16" /></button>
      <span class="terminal-keys-sep" aria-hidden="true" />
      <button type="button" :class="{ armed: moreKeys || tapSelect }" :aria-expanded="moreKeys" :aria-label="t('More keys')" :title="t('More keys')" @pointerdown="keepFocus" @click="moreKeys = !moreKeys"><Icon name="more" :size="16" /></button>
    </div>
  </div>
</template>
