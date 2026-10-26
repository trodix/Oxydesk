# Login screen theme

A client that connects without a password gets a login form drawn by the server. Its look can be changed with a theme
file, named by `greeter-theme` in `/etc/oxydesk/oxydesk.conf` (or `--greeter-theme FILE`):

```
greeter-theme = /etc/oxydesk/greeter.theme
```

The server reads the theme once, at start-up. A mistake in the file (syntax, unknown key, bad colour or number) stops
the server with the line to fix. A picture or font that cannot be read is only reported in the log, and the built-in
look is used instead, so a missing logo never keeps anybody from signing in.

## Format

`key = value` lines, as in the configuration file: `#` or `;` at the start of a line is a comment, a value may be wrapped
in double quotes, and every key is optional. Colours are `#rgb` or `#rrggbb`. Paths are relative to the directory of the
theme file. Sizes are in card units: the card is 400 units wide by default and the whole form is scaled to the screen.

| Key | Value | Default |
|---|---|---|
| `background` | one colour, or two (top and bottom of a gradient) | `#2c2537 #0c0b0a` |
| `background-glow` | `yes`/`no`: soft glow above the centre of the gradient | `yes` |
| `background-image` | PNG, scaled to cover the screen (centred, the overflow cut off) | none |
| `logo` | PNG shown at the top of the card instead of the badge with the initial of the name | none |
| `logo-size` | height of the logo, 16 to 200 (never wider than the card) | `76` |
| `card`, `card-border` | colours of the card | `#1d1a18`, `#2c2825` |
| `field`, `field-border` | colours of the input fields | `#141211`, `#3e3934` |
| `accent`, `accent-deep` | button (gradient), focused field and caret | `#ff8a4c`, `#d9480f` |
| `text`, `muted`, `error`, `button-text` | colours of the texts | `#f1ece6`, `#a39b93`, `#ff5c60`, `#fff` |
| `language` | `auto`, or `en` `fr` `de` `es` `it` `pt` `nl` | `auto` |
| `title`, `subtitle` | texts under the logo (`""` hides one) | `Oxydesk`, translated |
| `user-label`, `password-label` | labels of the fields | translated |
| `button-label`, `busy-label` | button, and button while the password is checked | translated |
| `footer` | notice under the card (legal mention, contact), at most 120 characters | none |
| `title.fr`, `footer.de`, … | the same texts for one language only; they win over the plain key | none |
| `font`, `font-bold` | TTF or OTF files; `font-bold` defaults to `font` | Noto Sans (Latin only) |
| `card-width` | 280 to 640 | `400` |
| `card-radius`, `field-radius` | corner radii, 0 to 40 and 0 to 22 | `18`, `8` |

Pictures are PNG only (any colour type; at most 8192 pixels per side and 32 MiB). The built-in font covers the Latin
alphabet: for another script, give a `font` that has its glyphs, or the characters show as empty boxes.

## Languages

The login screen is translated into English, French, German, Spanish, Italian, Portuguese and Dutch: the labels, the
button, the Caps Lock warning and the messages of a refused login. With `language = auto` (the default) the language
follows the keyboard layout the client announces, so a French keyboard gets French; a layout shared by several languages
(Belgian, for instance) or unknown gets English. `language = fr` fixes it whatever the keyboard.

A text of the theme replaces the translation. To keep one text per language, name the language in the key:

```
title = ACME Corp
subtitle = Remote desktop
subtitle.fr = Bureau à distance
subtitle.de = Fernzugriff
footer = Authorised use only
footer.fr = Usage réservé aux personnes autorisées
```

A text with no entry for the language in use falls back to the plain key, then to the built-in translation. The
built-in font has the accents of all these languages.

## Example

```
# /etc/oxydesk/greeter.theme
background = #0b1d33 #050b14
background-glow = no
logo = acme-logo.png
logo-size = 90
title = ACME Corp
subtitle = Remote desktop
accent = #2f9e44
accent-deep = #1b6b2d
footer = Authorised use only. Activity is logged.
```

Restart the server (`systemctl restart oxydesk`) to apply a change.
