<template>
  <template
    v-for="(segment, index) in segments"
    :key="index"
  ><span
    v-if="segment.kind === 'zwsp'"
    class="zwsp-marker"
    data-zwsp="1"
    :title="ZERO_WIDTH_PLACEHOLDER_TITLE"
  >{{ ZERO_WIDTH_PLACEHOLDER }}</span><template v-else>{{ segment.text }}</template></template>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import {
  splitZeroWidthSegments,
  ZERO_WIDTH_PLACEHOLDER,
  ZERO_WIDTH_PLACEHOLDER_TITLE,
} from '../../utils/zeroWidth'

const props = defineProps<{
  text: string
}>()

const segments = computed(() => splitZeroWidthSegments(props.text ?? ''))
</script>

<style>
.zwsp-marker {
  display: inline-block;
  padding: 0 0.15em;
  margin: 0 0.05em;
  border-radius: 3px;
  font-size: 0.7em;
  line-height: 1.2;
  vertical-align: 0.1em;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  color: rgb(180 83 9);
  background: rgb(251 191 36 / 0.25);
  border: 1px dashed rgb(217 119 6 / 0.6);
  cursor: help;
  user-select: none;
  white-space: nowrap;
}

.theme-dark .zwsp-marker,
.dark .zwsp-marker {
  color: rgb(252 211 77);
  background: rgb(217 119 6 / 0.2);
  border-color: rgb(245 158 11 / 0.6);
}
</style>
