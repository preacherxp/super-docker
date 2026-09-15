<script setup lang="ts">
import { ref } from "vue";
import demoUrl from "../../../docs/demo.gif?url";

const dialog = ref<HTMLDialogElement>();
const playing = ref(false);
function play() {
  playing.value = true;
  dialog.value?.showModal();
}
function closeOnBackdrop(event: MouseEvent) {
  if (event.target !== dialog.value || !dialog.value) return;
  const bounds = dialog.value.getBoundingClientRect();
  if (
    event.clientX < bounds.left ||
    event.clientX > bounds.right ||
    event.clientY < bounds.top ||
    event.clientY > bounds.bottom
  )
    dialog.value.close();
}
</script>

<template>
  <button class="text-button" id="watch-demo" @click="play">
    <span class="play-icon" aria-hidden="true">▷</span> See it in action
  </button>
  <dialog
    ref="dialog"
    id="demo-dialog"
    aria-labelledby="demo-title"
    @close="playing = false"
    @click="closeOnBackdrop"
  >
    <div class="dialog-heading">
      <div>
        <p class="eyebrow">THE REAL THING</p>
        <h2 id="demo-title">A minute in your element.</h2>
      </div>
      <button id="close-demo" aria-label="Close demo" @click="dialog?.close()">
        ×
      </button>
    </div>
    <div id="demo-media">
      <img
        v-if="playing"
        :src="demoUrl"
        alt="Actual super-docker recording showing a disposable nginx container, detail tabs, and Docker events."
        width="1200"
        height="760"
      />
    </div>
    <p class="demo-caption">
      Recorded in super-docker. Close this window to stop playback.
    </p>
  </dialog>
</template>
