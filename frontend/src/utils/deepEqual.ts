/**
 * 结构化深比较。
 *
 * 轮询接口每次都会返回内容相同、引用不同的对象；视图层需要用内容而不是引用来判断
 * “是否真的变化”，否则每次刷新都会重建组件并丢失滚动位置、折叠状态等交互状态。
 *
 * 规则：普通对象按键集合比较（与键顺序无关），数组按位置比较，其余值用 Object.is。
 * 非普通对象（Date、Map 等）只在引用相同时视为相等。可以直接比较 Vue 响应式代理。
 */
function isPlainObject(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null) return false
  const prototype: unknown = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

export function deepEqual(left: unknown, right: unknown): boolean {
  if (Object.is(left, right)) return true
  if (Array.isArray(left) || Array.isArray(right)) {
    if (!Array.isArray(left) || !Array.isArray(right) || left.length !== right.length) return false
    return left.every((item, index) => deepEqual(item, right[index]))
  }
  if (!isPlainObject(left) || !isPlainObject(right)) return false
  const leftKeys = Object.keys(left)
  if (leftKeys.length !== Object.keys(right).length) return false
  return leftKeys.every(key => Object.prototype.hasOwnProperty.call(right, key) && deepEqual(left[key], right[key]))
}
