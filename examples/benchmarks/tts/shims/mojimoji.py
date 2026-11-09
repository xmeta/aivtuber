"""Pure-Python stand-in for the two mojimoji calls misaki.ja makes (bench only; deviation recorded in EXP-0003).

The real `mojimoji` has no Windows wheel and needs a C compiler (Visual Studio Build Tools) to build, which the
bench machine did not have. misaki 0.7.4 ja.py calls exactly:
    mojimoji.zen_to_han(text, kana=False)           -> full-width ASCII letters/digits/symbols to half-width, kana untouched
    mojimoji.han_to_zen(text, digit=False, ascii=False) -> half-width katakana to full-width, digits/ASCII untouched
This shim reproduces those two behaviours with unicodedata; any other use raises.
"""
import unicodedata


def zen_to_han(text, kana=True, digit=True, ascii=True):
    if kana is not False or digit is not True or ascii is not True:
        raise NotImplementedError("shim only covers zen_to_han(text, kana=False)")
    out = []
    for ch in text:
        o = ord(ch)
        if 0xFF01 <= o <= 0xFF5E:
            out.append(chr(o - 0xFEE0))
        elif ch == "　":
            out.append(" ")
        else:
            out.append(ch)
    return "".join(out)


def han_to_zen(text, kana=True, digit=True, ascii=True):
    if kana is not True or digit is not False or ascii is not False:
        raise NotImplementedError("shim only covers han_to_zen(text, digit=False, ascii=False)")
    out = []
    for ch in text:
        if 0xFF61 <= ord(ch) <= 0xFF9F:  # half-width katakana block (dakuten marks are composed by NFKC on a pair basis)
            out.append(unicodedata.normalize("NFKC", ch))
        else:
            out.append(ch)
    return "".join(out)
