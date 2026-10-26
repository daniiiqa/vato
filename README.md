# vato

*Vato*: el compa que le sabe a todo y anda en todo el pedo. Escribes `vato`
en la terminal y te resuelve el audio, el bluetooth, la red y el sistema.

Parte de [discada](https://github.com/daniiiqa/dotfiles), hermano de
[rola](https://github.com/daniiiqa/rola): mismo look en cada estilo del rice
(bordes, barras, mayúsculas, cómo se marca lo elegido y los colores del
theme de kitty), en español o inglés según discada.

```
vato              audio
vato mic          audio, en las entradas
vato bluetooth    (o bt)
vato network      (o wifi)
vato system       (o sys)
```

| pestaña | qué hace | de dónde |
|---|---|---|
| **audio** | salidas, entradas, el volumen de cada app y quién usa el micrófono; predeterminada, mudo, mandar una app a otra salida o entrada | PipeWire (`pactl`) |
| **bluetooth** | emparejados y cercanos, conectar, emparejar, confianza, olvidar, batería | BlueZ por D-Bus (vato es su propio agente para emparejar) |
| **network** | cable, wifi y VPN (las guardadas también, para prenderlas); redes cerca con señal, conectar con contraseña, olvidar, wifi on/off | NetworkManager (`nmcli`) |
| **system** | CPU por núcleo y gráfica, GPU (AMD), VRAM, RAM, swap, discos, red, temperaturas, procesos (buscar con `/`, cerrar con `x`) | `/proc` y `/sys` |

Teclas: `1`–`4` o `tab` cambian de pestaña, `j`/`k` se mueven, `enter` hace
lo obvio de cada fila; la línea de abajo dice el resto. `q` sale.

## Instalar

```bash
cargo install --git https://github.com/daniiiqa/vato --root ~/.local --locked
```

El look sale de `~/.config/vato/look.json` o, si no existe, del de rola
(`~/.config/rola/look.json`), que cada look de discada ya trae. Las claves:
`borders` (auto, rounded, plain, double, thick, dashed, none), `bar` (line,
knob, blocks, dots, ascii), `case` (normal, upper, lower), `select` (fill,
bar, inverse), `tint` (color, phosphor, ink) y `transparent`.

---

*Vato*: the guy who knows how to handle everything. Audio, bluetooth, network
and system in the terminal, styled by each discada look like rola. English or
Spanish follows discada's language setting.
