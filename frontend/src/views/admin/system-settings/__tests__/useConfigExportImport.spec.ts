import { beforeEach, describe, expect, it, vi } from 'vitest'
import { ref } from 'vue'

const { errorMock, successMock } = vi.hoisted(() => ({
  errorMock: vi.fn(),
  successMock: vi.fn(),
}))

vi.mock('@/composables/useToast', () => ({
  useToast: () => ({
    error: errorMock,
    success: successMock,
  }),
}))

vi.mock('@/api/admin', () => ({
  adminApi: {},
}))

import { useConfigExportImport } from '../composables/useConfigExportImport'

function buildFileInputEvent(file: File): Event {
  return {
    target: {
      files: [file],
      value: 'selected.json',
    },
  } as unknown as Event
}

function makeSizedFile(content: string, size: number): File {
  const file = new File([content], 'config.json', { type: 'application/json' })
  Object.defineProperty(file, 'size', { value: size })
  return file
}

describe('useConfigExportImport file selection', () => {
  beforeEach(() => {
    errorMock.mockReset()
    successMock.mockReset()
  })

  it('accepts config import files larger than the old 10MB limit', async () => {
    const state = useConfigExportImport(ref({ site_name: 'Aether' }) as unknown as { value: import('../composables/useSystemConfig').SystemConfig })
    const file = makeSizedFile(
      JSON.stringify({
        version: '1',
        exported_at: '2026-01-01T00:00:00.000Z',
        global_models: [],
        providers: [],
      }),
      11 * 1024 * 1024
    )

    state.handleConfigFileSelect(buildFileInputEvent(file))
    await vi.waitFor(() => expect(state.importDialogOpen.value).toBe(true))

    expect(errorMock).not.toHaveBeenCalledWith('文件大小不能超过 10MB')
    expect(state.importPreview.value).toEqual({
      version: '1',
      exported_at: '2026-01-01T00:00:00.000Z',
      global_models: [],
      providers: [],
    })
    expect(state.importDialogOpen.value).toBe(true)
  })

  it('accepts config import files larger than the previous 500MB limit', async () => {
    const state = useConfigExportImport(ref({ site_name: 'Aether' }) as unknown as { value: import('../composables/useSystemConfig').SystemConfig })
    const file = makeSizedFile(
      JSON.stringify({
        version: '1',
        global_models: [],
      }),
      501 * 1024 * 1024
    )

    state.handleConfigFileSelect(buildFileInputEvent(file))
    await vi.waitFor(() => expect(state.importDialogOpen.value).toBe(true))

    expect(errorMock).not.toHaveBeenCalled()
    expect(state.importPreview.value).toEqual({
      version: '1',
      global_models: [],
    })
  })

  it('accepts user and aggregate imports larger than the previous 500MB limit', async () => {
    const usersState = useConfigExportImport(ref({ site_name: 'Aether' }) as unknown as { value: import('../composables/useSystemConfig').SystemConfig })
    const usersFile = makeSizedFile(
      JSON.stringify({
        version: '1.0',
        users: [],
      }),
      501 * 1024 * 1024
    )

    usersState.handleUsersFileSelect(buildFileInputEvent(usersFile))
    await vi.waitFor(() => expect(usersState.importUsersDialogOpen.value).toBe(true))

    const aggregateState = useConfigExportImport(ref({ site_name: 'Aether' }) as unknown as { value: import('../composables/useSystemConfig').SystemConfig })
    const aggregateFile = makeSizedFile(
      JSON.stringify({
        version: '1.0',
        config_data: { version: '1.0', global_models: [] },
        user_data: { version: '1.0', users: [] },
      }),
      501 * 1024 * 1024
    )

    aggregateState.handleAggregateFileSelect(buildFileInputEvent(aggregateFile))
    await vi.waitFor(() => expect(aggregateState.aggregateImportDialogOpen.value).toBe(true))

    expect(errorMock).not.toHaveBeenCalled()
  })
})
