<script setup lang="ts">
import { ref, watch } from 'vue'
import { ElMessage } from 'element-plus'

/**
 * One custom-header row.
 *
 * Element Plus rewrites an input's native value from `model-value` after every
 * keystroke. A row that only commits its name on `change` therefore never sees the
 * edit: the old `model-value` is pushed back into the field before the blur happens.
 * The name is kept as a local draft and committed on blur, which also avoids renaming
 * the record key on every keystroke and merging rows whose names transiently collide.
 */
const props = defineProps<{
  name: string
  value: string
  /** Names the other rows already hold; a duplicate would silently merge two headers. */
  taken: string[]
}>()

const emit = defineEmits<{
  rename: [next: string]
  'update:value': [next: string]
  remove: []
}>()

const draft = ref(props.name)

// The parent can rename or replace entries, so the draft follows the committed name.
watch(
  () => props.name,
  (next) => {
    if (next !== draft.value) draft.value = next
  },
)

function commit() {
  const next = draft.value.trim()
  if (!next) {
    // An unnamed header cannot be sent upstream.
    draft.value = props.name
    return
  }
  const duplicated = props.taken.some((name) => name.toLowerCase() === next.toLowerCase())
  if (next !== props.name && duplicated) {
    ElMessage.warning(`请求头「${next}」已存在`)
    draft.value = props.name
    return
  }
  draft.value = next
  if (next !== props.name) emit('rename', next)
}
</script>

<template>
  <div class="header-row">
    <el-input
      v-model="draft"
      size="small"
      placeholder="名称"
      @blur="commit"
      @keydown.enter="commit"
    />
    <el-input
      :model-value="value"
      size="small"
      placeholder="值"
      @update:model-value="(next: string) => emit('update:value', next)"
    />
    <el-button text size="small" type="danger" @click="emit('remove')">移除</el-button>
  </div>
</template>

<style scoped>
.header-row {
  display: grid;
  grid-template-columns: 220px 1fr auto;
  gap: 8px;
}
</style>
