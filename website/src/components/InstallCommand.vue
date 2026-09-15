<script setup lang="ts">
import { ref } from "vue";

const command =
  "cargo install --git https://github.com/preacherxp/super-docker super-docker";
const code = ref<HTMLElement>();
const copied = ref(false);
const message = ref("");

async function copyCommand() {
  try {
    await navigator.clipboard.writeText(command);
    copied.value = true;
    message.value = "Install command copied to clipboard.";
  } catch {
    if (code.value) {
      const range = document.createRange();
      range.selectNodeContents(code.value);
      const selection = window.getSelection();
      selection?.removeAllRanges();
      selection?.addRange(range);
    }
    message.value = "Command selected. Press Command+C or Control+C to copy.";
  }
}
</script>

<template>
  <div class="install-command">
    <span class="prompt" aria-hidden="true">$</span>
    <code ref="code" id="install-code">{{ command }}</code>
    <button
      id="copy-install"
      :aria-label="copied ? 'Install command copied' : 'Copy install command'"
      @click="copyCommand"
    >
      <svg
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="1.5"
        aria-hidden="true"
      >
        <path v-if="copied" d="m5 12 4 4L19 6" />
        <template v-else>
          <rect x="8" y="8" width="12" height="12" rx="2" />
          <path d="M16 8V4H4v12h4" />
        </template>
      </svg>
      <span>{{ copied ? "Copied" : "Copy" }}</span>
    </button>
  </div>
  <p class="copy-message" role="status" aria-live="polite">{{ message }}</p>
</template>

<style scoped>
.copy-message {
  font-size: 11px;
  color: var(--accent);
  margin-top: 8px;
}
.copy-message:empty {
  display: none;
}
</style>
