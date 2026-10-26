# vato

*Vato*: el compa que le sabe a todo y anda en todo el pedo. Escribes `vato`
en la terminal y te resuelve el audio, el bluetooth, la red, el sistema y las
notificaciones.

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
vato notifications  (o notif)
```

| pestaña | qué hace | de dónde |
|---|---|---|
| **audio** | salidas, entradas, el volumen de cada app y quién usa el micrófono; predeterminada, mudo, mandar una app a otra salida o entrada, puerto y perfil de la tarjeta (`p`) | PipeWire (`pactl`) |
| **bluetooth** | emparejados y cercanos, conectar, emparejar, confianza, olvidar, batería | BlueZ por D-Bus (vato es su propio agente: lo que pregunte un dispositivo al emparejar sale aquí y nada se acepta solo) |
| **network** | cable, wifi y VPN (las guardadas también, para prenderlas); redes cerca con señal, conectar con contraseña, olvidar, wifi on/off | NetworkManager (`nmcli`) |
| **system** | CPU por núcleo y gráfica; GPU con reloj, consumo, ventilador, temperaturas y qué procesos la usan (gráficos, video, VRAM); RAM, swap, discos, red; procesos (buscar con `/`, agrupar con `a`, cerrar con `x`, cerrar por nombre como pkill con `X`) | `/proc`, `/sys` y el fdinfo de DRM (AMD; por proceso también Intel) |
| **notifications** | el historial de [arre](https://github.com/daniiiqa/arre): abrir lo que pide cada una, quitarlas, borrar todas, no molestar y dónde salen | arre por D-Bus |

Teclas: `1`–`5` o `tab` cambian de pestaña, `j`/`k` se mueven, `enter` hace
lo obvio de cada fila; la línea de abajo dice el resto. `q` sale.

Ratón, como en rola: clic en una pestaña o en una tecla de la línea de abajo
hace lo mismo que la tecla; en las listas, clic elige, doble clic es `enter` y
clic derecho la otra acción (audio: mudo; bluetooth: confianza; red: olvidar;
procesos: cerrar). En audio, clic o arrastrar sobre la barra pone el volumen,
la rueda sobre una fila le sube o baja, y clic medio abre puerto/perfil. En
sistema, clic en `cpu` o `mem` ordena y clic en un proceso de la GPU lo busca
en la lista.

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
