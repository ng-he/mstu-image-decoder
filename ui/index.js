export default function mount(plugin) {
  const el = plugin.el

  /// The dot lights while images are still being made, not merely while a
  /// decoder is up: a stream can be decoding and changing nothing.
  let made = 0

  plugin.fit(320)

  /// Settings are numbers, and an empty field means the default rather than
  /// NaN.
  const number = (id, field) =>
    el(id).addEventListener('change', (event) => {
      const value = event.target.value === '' ? 0 : Number(event.target.value)

      if (Number.isFinite(value)) plugin.set(field, value)
    })

  number('width', 'width')
  number('height', 'height')
  number('quality', 'quality')
  number('rate', 'rate')
  number('change', 'min_change')
  number('heartbeat', 'heartbeat')

  el('keys').addEventListener('change', (event) => plugin.set('keyframes_only', event.target.checked))
  el('preview').addEventListener('change', (event) => plugin.set('preview', event.target.checked))
  el('decoder').addEventListener('change', (event) => plugin.set('decoder', event.target.value))

  plugin.onSettings((settings) => {
    el('width').value = settings.width || ''
    el('height').value = settings.height || ''
    el('quality').value = settings.quality ?? 80
    el('rate').value = settings.rate || ''
    el('change').value = settings.min_change ?? 0
    el('heartbeat').value = settings.heartbeat ?? 0
    el('keys').checked = settings.keyframes_only ?? false
    el('preview').checked = settings.preview ?? true
    el('decoder').value = settings.decoder || 'auto'
  })

  /// What it is decoding is only known once something upstream says so.
  plugin.onEvent('image.changed', ({ from, codec }) => {
    el('codec').textContent = from ? `${from} → ${codec}` : codec
    el('codec').hidden = !codec
  })

  plugin.onLive((live) => {
    const { state, frames, images, skipped, fps, backend, note, width, height, preview } = live

    el('shape').textContent = width ? `${width} by ${height}` : 'nothing decoded yet'

    /// The host serves the file the plugin keeps; the count makes the url
    /// change so the picture is fetched again rather than cached.
    if (preview) {
      el('shot').src = `${plugin.mediaUrl(preview)}&image=${images ?? 0}`
    }

    el('shot').hidden = !preview

    el('state').textContent = backend && backend !== 'none' ? `${state} · ${backend}` : state
    el('frames').textContent = frames ?? 0
    el('images').textContent = images ?? 0
    el('fps').textContent = fps ?? 0
    el('skipped').textContent = skipped ? `${skipped} let go` : 'nothing let go'

    el('note').textContent = note ?? ''
    el('note').hidden = !note

    el('dot').classList.toggle('on', state === 'decoding' && images !== made)

    made = images ?? 0
  })

  plugin.onState(({ running }) => {
    if (!running) el('dot').classList.remove('on')
  })
}
