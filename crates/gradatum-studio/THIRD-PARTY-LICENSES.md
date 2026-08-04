# Licences des composants tiers — gradatum-studio

Cette crate redistribue un bundle web precompile (`dist/`) qui **incorpore** du code et
des fontes tierces. Les notices ci-dessous sont reproduites **verbatim** depuis les
fichiers de licence des paquets effectivement embarques, conformement a la clause de
reproduction de notice de chaque licence (OFL-1.1 §2, MIT, ISC).

Le code Rust et les sources TypeScript de cette crate sont sous **Apache-2.0**
(voir `LICENSE` a la racine du depot). Expression SPDX de la crate :
`Apache-2.0 AND OFL-1.1 AND MIT AND ISC`.

## Perimetre — comment cette liste est etablie

Cloture des dependances de **production** calculee depuis `package-lock.json`
(entrees non marquees `dev`) : **77 paquets**. En sont retires les
**5 paquets `@types/*`** (@types/debug, @types/hast, @types/mdast, @types/ms, @types/unist), qui ne
contiennent qu'un `index.d.ts` : ce sont des declarations de types effacees a la
compilation, aucun octet ne se retrouve dans le bundle.

Restent **3 paquets de fontes** (OFL-1.1) et **69 paquets de code** 
(MIT / ISC). Les dependances de developpement (Vite, Vitest, TypeScript, jsdom,
testing-library...) ne sont pas redistribuees et n'apparaissent donc pas ici.

---

## 1. Fontes — SIL Open Font License 1.1

Trois familles de fontes sont embarquees dans `dist/assets/` aux formats `.woff` et
`.woff2`. Notices de copyright, reproduites depuis les fichiers `LICENSE` respectifs :

### @fontsource/ibm-plex-sans 5.2.8

```
Copyright 2019 IBM Corp. All rights reserved. IBMPlexSans-Italic[wdth,wght].ttf: Copyright 2019 IBM Corp. All rights reserved.
```

### @fontsource/jetbrains-mono 5.2.8

```
Copyright 2020 The JetBrains Mono Project Authors (https://github.com/JetBrains/JetBrainsMono) JetBrainsMono-Italic[wght].ttf: Copyright 2020 The JetBrains Mono Project Authors (https://github.com/JetBrains/JetBrainsMono)
```

### @fontsource/spectral 5.2.8

```
Copyright 2017 The Spectral Project Authors (https://github.com/productiontype/Spectral) Spectral-ExtraLightItalic.ttf: Copyright 2017 The Spectral Project Authors (https://github.com/productiontype/Spectral) Spectral-Light.ttf: Copyright 2017 The Spectral Project Authors (https://github.com/productiontype/Spectral) Spectral-LightItalic.ttf: Copyright 2017 The Spectral Project Authors (https://github.com/productiontype/Spectral) Spectral-Regular.ttf: Copyright 2017 The Spectral Project Authors (https://github.com/productiontype/Spectral) Spectral-Italic.ttf: Copyright 2017 The Spectral Project Authors (https://github.com/productiontype/Spectral) Spectral-Medium.ttf: Copyright 2017 The Spectral Project Authors (https://github.com/productiontype/Spectral) Spectral-MediumItalic.ttf: Copyright 2017 The Spectral Project Authors (https://github.com/productiontype/Spectral) Spectral-SemiBold.ttf: Copyright 2017 The Spectral Project Authors (https://github.com/productiontype/Spectral) Spectral-SemiBoldItalic.ttf: Copyright 2017 The Spectral Project Authors (https://github.com/productiontype/Spectral) Spectral-Bold.ttf: Copyright 2017 The Spectral Project Authors (https://github.com/productiontype/Spectral) Spectral-BoldItalic.ttf: Copyright 2017 The Spectral Project Authors (https://github.com/productiontype/Spectral) Spectral-ExtraBold.ttf: Copyright 2017 The Spectral Project Authors (https://github.com/productiontype/Spectral) Spectral-ExtraBoldItalic.ttf: Copyright 2017 The Spectral Project Authors (https://github.com/productiontype/Spectral)
```

Les trois paquets distribuent un texte de licence **identique** (verifie : meme
empreinte SHA-256). Il est donc reproduit une seule fois ci-dessous.

```
SIL OPEN FONT LICENSE Version 1.1 - 26 February 2007
-----------------------------------------------------------

PREAMBLE
The goals of the Open Font License (OFL) are to stimulate worldwide
development of collaborative font projects, to support the font creation
efforts of academic and linguistic communities, and to provide a free and
open framework in which fonts may be shared and improved in partnership
with others.

The OFL allows the licensed fonts to be used, studied, modified and
redistributed freely as long as they are not sold by themselves. The
fonts, including any derivative works, can be bundled, embedded,
redistributed and/or sold with any software provided that any reserved
names are not used by derivative works. The fonts and derivatives,
however, cannot be released under any other type of license. The
requirement for fonts to remain under this license does not apply
to any document created using the fonts or their derivatives.

DEFINITIONS
"Font Software" refers to the set of files released by the Copyright
Holder(s) under this license and clearly marked as such. This may
include source files, build scripts and documentation.

"Reserved Font Name" refers to any names specified as such after the
copyright statement(s).

"Original Version" refers to the collection of Font Software components as
distributed by the Copyright Holder(s).

"Modified Version" refers to any derivative made by adding to, deleting,
or substituting -- in part or in whole -- any of the components of the
Original Version, by changing formats or by porting the Font Software to a
new environment.

"Author" refers to any designer, engineer, programmer, technical
writer or other person who contributed to the Font Software.

PERMISSION & CONDITIONS
Permission is hereby granted, free of charge, to any person obtaining
a copy of the Font Software, to use, study, copy, merge, embed, modify,
redistribute, and sell modified and unmodified copies of the Font
Software, subject to the following conditions:

1) Neither the Font Software nor any of its individual components,
in Original or Modified Versions, may be sold by itself.

2) Original or Modified Versions of the Font Software may be bundled,
redistributed and/or sold with any software, provided that each copy
contains the above copyright notice and this license. These can be
included either as stand-alone text files, human-readable headers or
in the appropriate machine-readable metadata fields within text or
binary files as long as those fields can be easily viewed by the user.

3) No Modified Version of the Font Software may use the Reserved Font
Name(s) unless explicit written permission is granted by the corresponding
Copyright Holder. This restriction only applies to the primary font name as
presented to the users.

4) The name(s) of the Copyright Holder(s) or the Author(s) of the Font
Software shall not be used to promote, endorse or advertise any
Modified Version, except to acknowledge the contribution(s) of the
Copyright Holder(s) and the Author(s) or with their explicit written
permission.

5) The Font Software, modified or unmodified, in part or in whole,
must be distributed entirely under this license, and must not be
distributed under any other license. The requirement for fonts to
remain under this license does not apply to any document created
using the Font Software.

TERMINATION
This license becomes null and void if any of the above conditions are
not met.

DISCLAIMER
THE FONT SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO ANY WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT
OF COPYRIGHT, PATENT, TRADEMARK, OR OTHER RIGHT. IN NO EVENT SHALL THE
COPYRIGHT HOLDER BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
INCLUDING ANY GENERAL, SPECIAL, INDIRECT, INCIDENTAL, OR CONSEQUENTIAL
DAMAGES, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF THE USE OR INABILITY TO USE THE FONT SOFTWARE OR FROM
OTHER DEALINGS IN THE FONT SOFTWARE.
```

---

## 2. Code — MIT et ISC

Les 69 paquets ci-dessous sont incorpores au bundle JavaScript
(`dist/assets/index-*.js`, `vendor-*.js`). Ils sont regroupes par texte de licence :
les variantes de mise en forme d'un meme texte sont distinguees, chaque variante etant
reproduite integralement une fois, suivie des notices de copyright de chaque paquet.

### 2.1 — MIT (49 paquets)

- **bail** 2.0.2 — Copyright (c) 2015 Titus Wormer <tituswormer@gmail.com>
- **ccount** 2.0.1 — Copyright (c) 2015 Titus Wormer <tituswormer@gmail.com>
- **character-entities** 2.0.2 — Copyright (c) 2015 Titus Wormer <tituswormer@gmail.com>
- **character-entities-html4** 2.1.0 — Copyright (c) 2015 Titus Wormer <tituswormer@gmail.com>
- **character-entities-legacy** 3.0.0 — Copyright (c) 2015 Titus Wormer <tituswormer@gmail.com>
- **comma-separated-tokens** 2.0.3 — Copyright (c) 2016 Titus Wormer <tituswormer@gmail.com>
- **debug** 4.4.3 — Copyright (c) 2014-2017 TJ Holowaychuk <tj@vision-media.ca> / Copyright (c) 2018-2021 Josh Junon
- **decode-named-character-reference** 1.3.0 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **devlop** 1.1.0 — Copyright (c) 2023 Titus Wormer <tituswormer@gmail.com>
- **hast-util-sanitize** 5.0.2 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **hast-util-to-html** 9.0.5 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **hast-util-whitespace** 3.0.0 — Copyright (c) 2016 Titus Wormer <tituswormer@gmail.com>
- **html-void-elements** 3.0.0 — Copyright (c) 2016 Titus Wormer <tituswormer@gmail.com>
- **mdast-util-from-markdown** 2.0.3 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **mdast-util-to-hast** 13.2.1 — Copyright (c) 2016 Titus Wormer <tituswormer@gmail.com>
- **mdast-util-to-string** 4.0.0 — Copyright (c) 2015 Titus Wormer <tituswormer@gmail.com>
- **micromark** 4.0.2 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-core-commonmark** 2.0.3 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-factory-destination** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-factory-label** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-factory-space** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-factory-title** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-factory-whitespace** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-util-character** 2.1.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-util-chunked** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-util-classify-character** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-util-combine-extensions** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-util-decode-numeric-character-reference** 2.0.2 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-util-decode-string** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-util-encode** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-util-html-tag-name** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-util-normalize-identifier** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-util-resolve-all** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-util-sanitize-uri** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-util-subtokenize** 2.1.0 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-util-symbol** 2.0.1 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **micromark-util-types** 2.0.2 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **property-information** 7.2.0 — Copyright (c) Titus Wormer <mailto:tituswormer@gmail.com>
- **rehype-sanitize** 6.0.0 — Copyright (c) 2016 Titus Wormer <tituswormer@gmail.com>
- **remark-rehype** 11.1.2 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **space-separated-tokens** 2.0.2 — Copyright (c) 2016 Titus Wormer <tituswormer@gmail.com>
- **stringify-entities** 4.0.4 — Copyright (c) 2015 Titus Wormer <mailto:tituswormer@gmail.com>
- **trim-lines** 3.0.1 — Copyright (c) 2015 Titus Wormer <mailto:tituswormer@gmail.com>
- **unist-util-position** 5.0.0 — Copyright (c) 2015 Titus Wormer <tituswormer@gmail.com>
- **unist-util-stringify-position** 4.0.0 — Copyright (c) 2016 Titus Wormer <tituswormer@gmail.com>
- **unist-util-visit** 5.1.0 — Copyright (c) 2015 Titus Wormer <tituswormer@gmail.com>
- **unist-util-visit-parents** 6.0.2 — Copyright (c) 2016 Titus Wormer <tituswormer@gmail.com>
- **vfile-message** 4.0.3 — Copyright (c) Titus Wormer <tituswormer@gmail.com>
- **zwitch** 2.0.4 — Copyright (c) 2016 Titus Wormer <tituswormer@gmail.com>

```
(The MIT License)


Permission is hereby granted, free of charge, to any person obtaining
a copy of this software and associated documentation files (the
'Software'), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to
the following conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED 'AS IS', WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

### 2.2 — MIT (7 paquets)

- **@remix-run/router** 1.23.3 — Copyright (c) React Training LLC 2015-2019 / Copyright (c) Remix Software Inc. 2020-2021 / Copyright (c) Shopify Inc. 2022-2023
- **is-plain-obj** 4.1.0 — Copyright (c) Sindre Sorhus <sindresorhus@gmail.com> (https://sindresorhus.com)
- **react** 18.3.1 — Copyright (c) Facebook, Inc. and its affiliates.
- **react-dom** 18.3.1 — Copyright (c) Facebook, Inc. and its affiliates.
- **react-router** 6.30.4 — Copyright (c) React Training LLC 2015-2019 / Copyright (c) Remix Software Inc. 2020-2021 / Copyright (c) Shopify Inc. 2022-2023
- **react-router-dom** 6.30.4 — Copyright (c) React Training LLC 2015-2019 / Copyright (c) Remix Software Inc. 2020-2021 / Copyright (c) Shopify Inc. 2022-2023
- **scheduler** 0.23.2 — Copyright (c) Facebook, Inc. and its affiliates.

```
MIT License


Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

### 2.3 — MIT (6 paquets)

- **dequal** 2.0.3 — Copyright (c) Luke Edwards <luke.edwards05@gmail.com> (lukeed.com)
- **extend** 3.0.2 — Copyright (c) 2014 Stefan Thomas
- **js-tokens** 4.0.0 — Copyright (c) 2014, 2015, 2016, 2017, 2018 Simon Lydell
- **loose-envify** 1.4.0 — Copyright (c) 2015 Andres Suarez <zertosh@gmail.com>
- **ms** 2.1.3 — Copyright (c) 2020 Vercel, Inc.
- **uplot** 1.6.32 — Copyright (c) 2022 Leon Sorokin

```
The MIT License (MIT)


Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

### 2.4 — MIT (5 paquets)

- **rehype-stringify** 10.0.1 — Copyright (c) Titus Wormer
- **remark-parse** 11.0.0 — Copyright (c) 2014 Titus Wormer <tituswormer@gmail.com>
- **trough** 2.2.0 — Copyright (c) 2016 Titus Wormer <tituswormer@gmail.com>
- **unified** 11.0.5 — Copyright (c) 2015 Titus Wormer <tituswormer@gmail.com>
- **vfile** 6.0.3 — Copyright (c) 2015 Titus Wormer <tituswormer@gmail.com>

```
(The MIT License)


Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

### 2.5 — ISC (1 paquet)

- **@ungap/structured-clone** 1.3.1 — Copyright (c) 2021, Andrea Giammarchi, @WebReflection

```
ISC License


Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH
REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY
AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT,
INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM
LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE
OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
```

### 2.6 — MIT (1 paquet)

- **unist-util-is** 6.0.1 — Copyright (c) 2015 Titus Wormer <tituswormer@gmail.com>

```
(The MIT license)


Permission is hereby granted, free of charge, to any person obtaining
a copy of this software and associated documentation files (the
'Software'), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to
the following conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED 'AS IS', WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

