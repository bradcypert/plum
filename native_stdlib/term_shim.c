// The terminal platform layer: is this a terminal, how big is it, and
// writing to it (issue #3).
//
// Deliberately the SMALL half of that issue. Turning input bytes into
// semantic key events is #35, and it is pure Plum with no C in it at
// all -- which is exactly why it is not here. What needs a shim is what
// cannot be done from Plum: asking the operating system a question
// about a file descriptor.
//
// Same ABI conventions as its neighbours: every function takes and
// returns only `long long`, or a `const char *` for text.
//
// **The size is a QUERY then two READS**, the same split
// `tcp_recv_n`/`tcp_recv_data` and `file_read_n`/`file_read_data` use,
// and for the same reason: Plum's extern surface has no multi-value
// return, and a terminal size is two numbers that must come from ONE
// observation. Asking for the width and the height separately would
// let a resize land between them and report a size the terminal never
// had.

#include <stdio.h>
#include <stdint.h>
#include <string.h>

struct plum_term_interval { uint32_t first, last; };

/* Generated from Unicode 16.0.0 property data. */
static const struct plum_term_interval plum_term_wide[] = {
    { 0x1100, 0x115F },
    { 0x231A, 0x231B },
    { 0x2329, 0x232A },
    { 0x23E9, 0x23EC },
    { 0x23F0, 0x23F0 },
    { 0x23F3, 0x23F3 },
    { 0x25FD, 0x25FE },
    { 0x2614, 0x2615 },
    { 0x2630, 0x2637 },
    { 0x2648, 0x2653 },
    { 0x267F, 0x267F },
    { 0x268A, 0x268F },
    { 0x2693, 0x2693 },
    { 0x26A1, 0x26A1 },
    { 0x26AA, 0x26AB },
    { 0x26BD, 0x26BE },
    { 0x26C4, 0x26C5 },
    { 0x26CE, 0x26CE },
    { 0x26D4, 0x26D4 },
    { 0x26EA, 0x26EA },
    { 0x26F2, 0x26F3 },
    { 0x26F5, 0x26F5 },
    { 0x26FA, 0x26FA },
    { 0x26FD, 0x26FD },
    { 0x2705, 0x2705 },
    { 0x270A, 0x270B },
    { 0x2728, 0x2728 },
    { 0x274C, 0x274C },
    { 0x274E, 0x274E },
    { 0x2753, 0x2755 },
    { 0x2757, 0x2757 },
    { 0x2795, 0x2797 },
    { 0x27B0, 0x27B0 },
    { 0x27BF, 0x27BF },
    { 0x2B1B, 0x2B1C },
    { 0x2B50, 0x2B50 },
    { 0x2B55, 0x2B55 },
    { 0x2E80, 0x2E99 },
    { 0x2E9B, 0x2EF3 },
    { 0x2F00, 0x2FD5 },
    { 0x2FF0, 0x2FFF },
    { 0x3000, 0x3000 },
    { 0x3001, 0x303E },
    { 0x3041, 0x3096 },
    { 0x3099, 0x30FF },
    { 0x3105, 0x312F },
    { 0x3131, 0x318E },
    { 0x3190, 0x31E5 },
    { 0x31EF, 0x321E },
    { 0x3220, 0x3247 },
    { 0x3250, 0xA48C },
    { 0xA490, 0xA4C6 },
    { 0xA960, 0xA97C },
    { 0xAC00, 0xD7A3 },
    { 0xF900, 0xFAFF },
    { 0xFE10, 0xFE19 },
    { 0xFE30, 0xFE52 },
    { 0xFE54, 0xFE66 },
    { 0xFE68, 0xFE6B },
    { 0xFF01, 0xFF60 },
    { 0xFFE0, 0xFFE6 },
    { 0x16FE0, 0x16FE4 },
    { 0x16FF0, 0x16FF1 },
    { 0x17000, 0x187F7 },
    { 0x18800, 0x18CD5 },
    { 0x18CFF, 0x18D08 },
    { 0x1AFF0, 0x1AFF3 },
    { 0x1AFF5, 0x1AFFB },
    { 0x1AFFD, 0x1AFFE },
    { 0x1B000, 0x1B122 },
    { 0x1B132, 0x1B132 },
    { 0x1B150, 0x1B152 },
    { 0x1B155, 0x1B155 },
    { 0x1B164, 0x1B167 },
    { 0x1B170, 0x1B2FB },
    { 0x1D300, 0x1D356 },
    { 0x1D360, 0x1D376 },
    { 0x1F004, 0x1F004 },
    { 0x1F0CF, 0x1F0CF },
    { 0x1F18E, 0x1F18E },
    { 0x1F191, 0x1F19A },
    { 0x1F1E6, 0x1F1FF },
    { 0x1F200, 0x1F202 },
    { 0x1F201, 0x1F201 },
    { 0x1F210, 0x1F23B },
    { 0x1F21A, 0x1F21A },
    { 0x1F22F, 0x1F22F },
    { 0x1F232, 0x1F236 },
    { 0x1F238, 0x1F23A },
    { 0x1F240, 0x1F248 },
    { 0x1F250, 0x1F251 },
    { 0x1F260, 0x1F265 },
    { 0x1F300, 0x1F320 },
    { 0x1F32D, 0x1F335 },
    { 0x1F337, 0x1F37C },
    { 0x1F37E, 0x1F393 },
    { 0x1F3A0, 0x1F3CA },
    { 0x1F3CF, 0x1F3D3 },
    { 0x1F3E0, 0x1F3F0 },
    { 0x1F3F4, 0x1F3F4 },
    { 0x1F3F8, 0x1F43E },
    { 0x1F440, 0x1F440 },
    { 0x1F442, 0x1F4FC },
    { 0x1F4FF, 0x1F53D },
    { 0x1F54B, 0x1F54E },
    { 0x1F550, 0x1F567 },
    { 0x1F57A, 0x1F57A },
    { 0x1F595, 0x1F596 },
    { 0x1F5A4, 0x1F5A4 },
    { 0x1F5FB, 0x1F64F },
    { 0x1F680, 0x1F6C5 },
    { 0x1F6CC, 0x1F6CC },
    { 0x1F6D0, 0x1F6D2 },
    { 0x1F6D5, 0x1F6D7 },
    { 0x1F6DC, 0x1F6DF },
    { 0x1F6EB, 0x1F6EC },
    { 0x1F6F4, 0x1F6FC },
    { 0x1F7E0, 0x1F7EB },
    { 0x1F7F0, 0x1F7F0 },
    { 0x1F90C, 0x1F93A },
    { 0x1F93C, 0x1F945 },
    { 0x1F947, 0x1F9FF },
    { 0x1FA70, 0x1FA7C },
    { 0x1FA80, 0x1FA89 },
    { 0x1FA8F, 0x1FAC6 },
    { 0x1FACE, 0x1FADC },
    { 0x1FADF, 0x1FAE9 },
    { 0x1FAF0, 0x1FAF8 },
    { 0x20000, 0x2FFFD },
    { 0x30000, 0x3FFFD },
};
static const size_t plum_term_wide_len = sizeof(plum_term_wide) / sizeof(plum_term_wide[0]);

static const struct plum_term_interval plum_term_extend[] = {
    { 0x300, 0x36F },
    { 0x483, 0x489 },
    { 0x591, 0x5BD },
    { 0x5BF, 0x5BF },
    { 0x5C1, 0x5C2 },
    { 0x5C4, 0x5C5 },
    { 0x5C7, 0x5C7 },
    { 0x610, 0x61A },
    { 0x64B, 0x65F },
    { 0x670, 0x670 },
    { 0x6D6, 0x6DC },
    { 0x6DF, 0x6E4 },
    { 0x6E7, 0x6E8 },
    { 0x6EA, 0x6ED },
    { 0x711, 0x711 },
    { 0x730, 0x74A },
    { 0x7A6, 0x7B0 },
    { 0x7EB, 0x7F3 },
    { 0x7FD, 0x7FD },
    { 0x816, 0x819 },
    { 0x81B, 0x823 },
    { 0x825, 0x827 },
    { 0x829, 0x82D },
    { 0x859, 0x85B },
    { 0x897, 0x89F },
    { 0x8CA, 0x8E1 },
    { 0x8E3, 0x902 },
    { 0x93A, 0x93A },
    { 0x93C, 0x93C },
    { 0x941, 0x948 },
    { 0x94D, 0x94D },
    { 0x951, 0x957 },
    { 0x962, 0x963 },
    { 0x981, 0x981 },
    { 0x9BC, 0x9BC },
    { 0x9BE, 0x9BE },
    { 0x9C1, 0x9C4 },
    { 0x9CD, 0x9CD },
    { 0x9D7, 0x9D7 },
    { 0x9E2, 0x9E3 },
    { 0x9FE, 0x9FE },
    { 0xA01, 0xA02 },
    { 0xA3C, 0xA3C },
    { 0xA41, 0xA42 },
    { 0xA47, 0xA48 },
    { 0xA4B, 0xA4D },
    { 0xA51, 0xA51 },
    { 0xA70, 0xA71 },
    { 0xA75, 0xA75 },
    { 0xA81, 0xA82 },
    { 0xABC, 0xABC },
    { 0xAC1, 0xAC5 },
    { 0xAC7, 0xAC8 },
    { 0xACD, 0xACD },
    { 0xAE2, 0xAE3 },
    { 0xAFA, 0xAFF },
    { 0xB01, 0xB01 },
    { 0xB3C, 0xB3C },
    { 0xB3E, 0xB3F },
    { 0xB41, 0xB44 },
    { 0xB4D, 0xB4D },
    { 0xB55, 0xB57 },
    { 0xB62, 0xB63 },
    { 0xB82, 0xB82 },
    { 0xBBE, 0xBBE },
    { 0xBC0, 0xBC0 },
    { 0xBCD, 0xBCD },
    { 0xBD7, 0xBD7 },
    { 0xC00, 0xC00 },
    { 0xC04, 0xC04 },
    { 0xC3C, 0xC3C },
    { 0xC3E, 0xC40 },
    { 0xC46, 0xC48 },
    { 0xC4A, 0xC4D },
    { 0xC55, 0xC56 },
    { 0xC62, 0xC63 },
    { 0xC81, 0xC81 },
    { 0xCBC, 0xCBC },
    { 0xCBF, 0xCC0 },
    { 0xCC2, 0xCC2 },
    { 0xCC6, 0xCC8 },
    { 0xCCA, 0xCCD },
    { 0xCD5, 0xCD6 },
    { 0xCE2, 0xCE3 },
    { 0xD00, 0xD01 },
    { 0xD3B, 0xD3C },
    { 0xD3E, 0xD3E },
    { 0xD41, 0xD44 },
    { 0xD4D, 0xD4D },
    { 0xD57, 0xD57 },
    { 0xD62, 0xD63 },
    { 0xD81, 0xD81 },
    { 0xDCA, 0xDCA },
    { 0xDCF, 0xDCF },
    { 0xDD2, 0xDD4 },
    { 0xDD6, 0xDD6 },
    { 0xDDF, 0xDDF },
    { 0xE31, 0xE31 },
    { 0xE34, 0xE3A },
    { 0xE47, 0xE4E },
    { 0xEB1, 0xEB1 },
    { 0xEB4, 0xEBC },
    { 0xEC8, 0xECE },
    { 0xF18, 0xF19 },
    { 0xF35, 0xF35 },
    { 0xF37, 0xF37 },
    { 0xF39, 0xF39 },
    { 0xF71, 0xF7E },
    { 0xF80, 0xF84 },
    { 0xF86, 0xF87 },
    { 0xF8D, 0xF97 },
    { 0xF99, 0xFBC },
    { 0xFC6, 0xFC6 },
    { 0x102D, 0x1030 },
    { 0x1032, 0x1037 },
    { 0x1039, 0x103A },
    { 0x103D, 0x103E },
    { 0x1058, 0x1059 },
    { 0x105E, 0x1060 },
    { 0x1071, 0x1074 },
    { 0x1082, 0x1082 },
    { 0x1085, 0x1086 },
    { 0x108D, 0x108D },
    { 0x109D, 0x109D },
    { 0x135D, 0x135F },
    { 0x1712, 0x1715 },
    { 0x1732, 0x1734 },
    { 0x1752, 0x1753 },
    { 0x1772, 0x1773 },
    { 0x17B4, 0x17B5 },
    { 0x17B7, 0x17BD },
    { 0x17C6, 0x17C6 },
    { 0x17C9, 0x17D3 },
    { 0x17DD, 0x17DD },
    { 0x180B, 0x180D },
    { 0x180F, 0x180F },
    { 0x1885, 0x1886 },
    { 0x18A9, 0x18A9 },
    { 0x1920, 0x1922 },
    { 0x1927, 0x1928 },
    { 0x1932, 0x1932 },
    { 0x1939, 0x193B },
    { 0x1A17, 0x1A18 },
    { 0x1A1B, 0x1A1B },
    { 0x1A56, 0x1A56 },
    { 0x1A58, 0x1A5E },
    { 0x1A60, 0x1A60 },
    { 0x1A62, 0x1A62 },
    { 0x1A65, 0x1A6C },
    { 0x1A73, 0x1A7C },
    { 0x1A7F, 0x1A7F },
    { 0x1AB0, 0x1ACE },
    { 0x1B00, 0x1B03 },
    { 0x1B34, 0x1B3D },
    { 0x1B42, 0x1B44 },
    { 0x1B6B, 0x1B73 },
    { 0x1B80, 0x1B81 },
    { 0x1BA2, 0x1BA5 },
    { 0x1BA8, 0x1BAD },
    { 0x1BE6, 0x1BE6 },
    { 0x1BE8, 0x1BE9 },
    { 0x1BED, 0x1BED },
    { 0x1BEF, 0x1BF3 },
    { 0x1C2C, 0x1C33 },
    { 0x1C36, 0x1C37 },
    { 0x1CD0, 0x1CD2 },
    { 0x1CD4, 0x1CE0 },
    { 0x1CE2, 0x1CE8 },
    { 0x1CED, 0x1CED },
    { 0x1CF4, 0x1CF4 },
    { 0x1CF8, 0x1CF9 },
    { 0x1DC0, 0x1DFF },
    { 0x200C, 0x200C },
    { 0x20D0, 0x20F0 },
    { 0x2CEF, 0x2CF1 },
    { 0x2D7F, 0x2D7F },
    { 0x2DE0, 0x2DFF },
    { 0x302A, 0x302F },
    { 0x3099, 0x309A },
    { 0xA66F, 0xA672 },
    { 0xA674, 0xA67D },
    { 0xA69E, 0xA69F },
    { 0xA6F0, 0xA6F1 },
    { 0xA802, 0xA802 },
    { 0xA806, 0xA806 },
    { 0xA80B, 0xA80B },
    { 0xA825, 0xA826 },
    { 0xA82C, 0xA82C },
    { 0xA8C4, 0xA8C5 },
    { 0xA8E0, 0xA8F1 },
    { 0xA8FF, 0xA8FF },
    { 0xA926, 0xA92D },
    { 0xA947, 0xA951 },
    { 0xA953, 0xA953 },
    { 0xA980, 0xA982 },
    { 0xA9B3, 0xA9B3 },
    { 0xA9B6, 0xA9B9 },
    { 0xA9BC, 0xA9BD },
    { 0xA9C0, 0xA9C0 },
    { 0xA9E5, 0xA9E5 },
    { 0xAA29, 0xAA2E },
    { 0xAA31, 0xAA32 },
    { 0xAA35, 0xAA36 },
    { 0xAA43, 0xAA43 },
    { 0xAA4C, 0xAA4C },
    { 0xAA7C, 0xAA7C },
    { 0xAAB0, 0xAAB0 },
    { 0xAAB2, 0xAAB4 },
    { 0xAAB7, 0xAAB8 },
    { 0xAABE, 0xAABF },
    { 0xAAC1, 0xAAC1 },
    { 0xAAEC, 0xAAED },
    { 0xAAF6, 0xAAF6 },
    { 0xABE5, 0xABE5 },
    { 0xABE8, 0xABE8 },
    { 0xABED, 0xABED },
    { 0xFB1E, 0xFB1E },
    { 0xFE00, 0xFE0F },
    { 0xFE20, 0xFE2F },
    { 0xFF9E, 0xFF9F },
    { 0x101FD, 0x101FD },
    { 0x102E0, 0x102E0 },
    { 0x10376, 0x1037A },
    { 0x10A01, 0x10A03 },
    { 0x10A05, 0x10A06 },
    { 0x10A0C, 0x10A0F },
    { 0x10A38, 0x10A3A },
    { 0x10A3F, 0x10A3F },
    { 0x10AE5, 0x10AE6 },
    { 0x10D24, 0x10D27 },
    { 0x10D69, 0x10D6D },
    { 0x10EAB, 0x10EAC },
    { 0x10EFC, 0x10EFF },
    { 0x10F46, 0x10F50 },
    { 0x10F82, 0x10F85 },
    { 0x11001, 0x11001 },
    { 0x11038, 0x11046 },
    { 0x11070, 0x11070 },
    { 0x11073, 0x11074 },
    { 0x1107F, 0x11081 },
    { 0x110B3, 0x110B6 },
    { 0x110B9, 0x110BA },
    { 0x110C2, 0x110C2 },
    { 0x11100, 0x11102 },
    { 0x11127, 0x1112B },
    { 0x1112D, 0x11134 },
    { 0x11173, 0x11173 },
    { 0x11180, 0x11181 },
    { 0x111B6, 0x111BE },
    { 0x111C0, 0x111C0 },
    { 0x111C9, 0x111CC },
    { 0x111CF, 0x111CF },
    { 0x1122F, 0x11231 },
    { 0x11234, 0x11237 },
    { 0x1123E, 0x1123E },
    { 0x11241, 0x11241 },
    { 0x112DF, 0x112DF },
    { 0x112E3, 0x112EA },
    { 0x11300, 0x11301 },
    { 0x1133B, 0x1133C },
    { 0x1133E, 0x1133E },
    { 0x11340, 0x11340 },
    { 0x1134D, 0x1134D },
    { 0x11357, 0x11357 },
    { 0x11366, 0x1136C },
    { 0x11370, 0x11374 },
    { 0x113B8, 0x113B8 },
    { 0x113BB, 0x113C0 },
    { 0x113C2, 0x113C2 },
    { 0x113C5, 0x113C5 },
    { 0x113C7, 0x113C9 },
    { 0x113CE, 0x113D0 },
    { 0x113D2, 0x113D2 },
    { 0x113E1, 0x113E2 },
    { 0x11438, 0x1143F },
    { 0x11442, 0x11444 },
    { 0x11446, 0x11446 },
    { 0x1145E, 0x1145E },
    { 0x114B0, 0x114B0 },
    { 0x114B3, 0x114B8 },
    { 0x114BA, 0x114BA },
    { 0x114BD, 0x114BD },
    { 0x114BF, 0x114C0 },
    { 0x114C2, 0x114C3 },
    { 0x115AF, 0x115AF },
    { 0x115B2, 0x115B5 },
    { 0x115BC, 0x115BD },
    { 0x115BF, 0x115C0 },
    { 0x115DC, 0x115DD },
    { 0x11633, 0x1163A },
    { 0x1163D, 0x1163D },
    { 0x1163F, 0x11640 },
    { 0x116AB, 0x116AB },
    { 0x116AD, 0x116AD },
    { 0x116B0, 0x116B7 },
    { 0x1171D, 0x1171D },
    { 0x1171F, 0x1171F },
    { 0x11722, 0x11725 },
    { 0x11727, 0x1172B },
    { 0x1182F, 0x11837 },
    { 0x11839, 0x1183A },
    { 0x11930, 0x11930 },
    { 0x1193B, 0x1193E },
    { 0x11943, 0x11943 },
    { 0x119D4, 0x119D7 },
    { 0x119DA, 0x119DB },
    { 0x119E0, 0x119E0 },
    { 0x11A01, 0x11A0A },
    { 0x11A33, 0x11A38 },
    { 0x11A3B, 0x11A3E },
    { 0x11A47, 0x11A47 },
    { 0x11A51, 0x11A56 },
    { 0x11A59, 0x11A5B },
    { 0x11A8A, 0x11A96 },
    { 0x11A98, 0x11A99 },
    { 0x11C30, 0x11C36 },
    { 0x11C38, 0x11C3D },
    { 0x11C3F, 0x11C3F },
    { 0x11C92, 0x11CA7 },
    { 0x11CAA, 0x11CB0 },
    { 0x11CB2, 0x11CB3 },
    { 0x11CB5, 0x11CB6 },
    { 0x11D31, 0x11D36 },
    { 0x11D3A, 0x11D3A },
    { 0x11D3C, 0x11D3D },
    { 0x11D3F, 0x11D45 },
    { 0x11D47, 0x11D47 },
    { 0x11D90, 0x11D91 },
    { 0x11D95, 0x11D95 },
    { 0x11D97, 0x11D97 },
    { 0x11EF3, 0x11EF4 },
    { 0x11F00, 0x11F01 },
    { 0x11F36, 0x11F3A },
    { 0x11F40, 0x11F42 },
    { 0x11F5A, 0x11F5A },
    { 0x13440, 0x13440 },
    { 0x13447, 0x13455 },
    { 0x1611E, 0x16129 },
    { 0x1612D, 0x1612F },
    { 0x16AF0, 0x16AF4 },
    { 0x16B30, 0x16B36 },
    { 0x16F4F, 0x16F4F },
    { 0x16F8F, 0x16F92 },
    { 0x16FE4, 0x16FE4 },
    { 0x16FF0, 0x16FF1 },
    { 0x1BC9D, 0x1BC9E },
    { 0x1CF00, 0x1CF2D },
    { 0x1CF30, 0x1CF46 },
    { 0x1D165, 0x1D169 },
    { 0x1D16D, 0x1D172 },
    { 0x1D17B, 0x1D182 },
    { 0x1D185, 0x1D18B },
    { 0x1D1AA, 0x1D1AD },
    { 0x1D242, 0x1D244 },
    { 0x1DA00, 0x1DA36 },
    { 0x1DA3B, 0x1DA6C },
    { 0x1DA75, 0x1DA75 },
    { 0x1DA84, 0x1DA84 },
    { 0x1DA9B, 0x1DA9F },
    { 0x1DAA1, 0x1DAAF },
    { 0x1E000, 0x1E006 },
    { 0x1E008, 0x1E018 },
    { 0x1E01B, 0x1E021 },
    { 0x1E023, 0x1E024 },
    { 0x1E026, 0x1E02A },
    { 0x1E08F, 0x1E08F },
    { 0x1E130, 0x1E136 },
    { 0x1E2AE, 0x1E2AE },
    { 0x1E2EC, 0x1E2EF },
    { 0x1E4EC, 0x1E4EF },
    { 0x1E5EE, 0x1E5EF },
    { 0x1E8D0, 0x1E8D6 },
    { 0x1E944, 0x1E94A },
    { 0x1F3FB, 0x1F3FF },
    { 0xE0020, 0xE007F },
    { 0xE0100, 0xE01EF },
};
static const size_t plum_term_extend_len = sizeof(plum_term_extend) / sizeof(plum_term_extend[0]);

static const struct plum_term_interval plum_term_spacing_mark[] = {
    { 0x903, 0x903 },
    { 0x93B, 0x93B },
    { 0x93E, 0x940 },
    { 0x949, 0x94C },
    { 0x94E, 0x94F },
    { 0x982, 0x983 },
    { 0x9BF, 0x9C0 },
    { 0x9C7, 0x9C8 },
    { 0x9CB, 0x9CC },
    { 0xA03, 0xA03 },
    { 0xA3E, 0xA40 },
    { 0xA83, 0xA83 },
    { 0xABE, 0xAC0 },
    { 0xAC9, 0xAC9 },
    { 0xACB, 0xACC },
    { 0xB02, 0xB03 },
    { 0xB40, 0xB40 },
    { 0xB47, 0xB48 },
    { 0xB4B, 0xB4C },
    { 0xBBF, 0xBBF },
    { 0xBC1, 0xBC2 },
    { 0xBC6, 0xBC8 },
    { 0xBCA, 0xBCC },
    { 0xC01, 0xC03 },
    { 0xC41, 0xC44 },
    { 0xC82, 0xC83 },
    { 0xCBE, 0xCBE },
    { 0xCC1, 0xCC1 },
    { 0xCC3, 0xCC4 },
    { 0xCF3, 0xCF3 },
    { 0xD02, 0xD03 },
    { 0xD3F, 0xD40 },
    { 0xD46, 0xD48 },
    { 0xD4A, 0xD4C },
    { 0xD82, 0xD83 },
    { 0xDD0, 0xDD1 },
    { 0xDD8, 0xDDE },
    { 0xDF2, 0xDF3 },
    { 0xE33, 0xE33 },
    { 0xEB3, 0xEB3 },
    { 0xF3E, 0xF3F },
    { 0xF7F, 0xF7F },
    { 0x1031, 0x1031 },
    { 0x103B, 0x103C },
    { 0x1056, 0x1057 },
    { 0x1084, 0x1084 },
    { 0x17B6, 0x17B6 },
    { 0x17BE, 0x17C5 },
    { 0x17C7, 0x17C8 },
    { 0x1923, 0x1926 },
    { 0x1929, 0x192B },
    { 0x1930, 0x1931 },
    { 0x1933, 0x1938 },
    { 0x1A19, 0x1A1A },
    { 0x1A55, 0x1A55 },
    { 0x1A57, 0x1A57 },
    { 0x1A6D, 0x1A72 },
    { 0x1B04, 0x1B04 },
    { 0x1B3E, 0x1B41 },
    { 0x1B82, 0x1B82 },
    { 0x1BA1, 0x1BA1 },
    { 0x1BA6, 0x1BA7 },
    { 0x1BE7, 0x1BE7 },
    { 0x1BEA, 0x1BEC },
    { 0x1BEE, 0x1BEE },
    { 0x1C24, 0x1C2B },
    { 0x1C34, 0x1C35 },
    { 0x1CE1, 0x1CE1 },
    { 0x1CF7, 0x1CF7 },
    { 0xA823, 0xA824 },
    { 0xA827, 0xA827 },
    { 0xA880, 0xA881 },
    { 0xA8B4, 0xA8C3 },
    { 0xA952, 0xA952 },
    { 0xA983, 0xA983 },
    { 0xA9B4, 0xA9B5 },
    { 0xA9BA, 0xA9BB },
    { 0xA9BE, 0xA9BF },
    { 0xAA2F, 0xAA30 },
    { 0xAA33, 0xAA34 },
    { 0xAA4D, 0xAA4D },
    { 0xAAEB, 0xAAEB },
    { 0xAAEE, 0xAAEF },
    { 0xAAF5, 0xAAF5 },
    { 0xABE3, 0xABE4 },
    { 0xABE6, 0xABE7 },
    { 0xABE9, 0xABEA },
    { 0xABEC, 0xABEC },
    { 0x11000, 0x11000 },
    { 0x11002, 0x11002 },
    { 0x11082, 0x11082 },
    { 0x110B0, 0x110B2 },
    { 0x110B7, 0x110B8 },
    { 0x1112C, 0x1112C },
    { 0x11145, 0x11146 },
    { 0x11182, 0x11182 },
    { 0x111B3, 0x111B5 },
    { 0x111BF, 0x111BF },
    { 0x111CE, 0x111CE },
    { 0x1122C, 0x1122E },
    { 0x11232, 0x11233 },
    { 0x112E0, 0x112E2 },
    { 0x11302, 0x11303 },
    { 0x1133F, 0x1133F },
    { 0x11341, 0x11344 },
    { 0x11347, 0x11348 },
    { 0x1134B, 0x1134C },
    { 0x11362, 0x11363 },
    { 0x113B9, 0x113BA },
    { 0x113CA, 0x113CA },
    { 0x113CC, 0x113CD },
    { 0x11435, 0x11437 },
    { 0x11440, 0x11441 },
    { 0x11445, 0x11445 },
    { 0x114B1, 0x114B2 },
    { 0x114B9, 0x114B9 },
    { 0x114BB, 0x114BC },
    { 0x114BE, 0x114BE },
    { 0x114C1, 0x114C1 },
    { 0x115B0, 0x115B1 },
    { 0x115B8, 0x115BB },
    { 0x115BE, 0x115BE },
    { 0x11630, 0x11632 },
    { 0x1163B, 0x1163C },
    { 0x1163E, 0x1163E },
    { 0x116AC, 0x116AC },
    { 0x116AE, 0x116AF },
    { 0x1171E, 0x1171E },
    { 0x11726, 0x11726 },
    { 0x1182C, 0x1182E },
    { 0x11838, 0x11838 },
    { 0x11931, 0x11935 },
    { 0x11937, 0x11938 },
    { 0x11940, 0x11940 },
    { 0x11942, 0x11942 },
    { 0x119D1, 0x119D3 },
    { 0x119DC, 0x119DF },
    { 0x119E4, 0x119E4 },
    { 0x11A39, 0x11A39 },
    { 0x11A57, 0x11A58 },
    { 0x11A97, 0x11A97 },
    { 0x11C2F, 0x11C2F },
    { 0x11C3E, 0x11C3E },
    { 0x11CA9, 0x11CA9 },
    { 0x11CB1, 0x11CB1 },
    { 0x11CB4, 0x11CB4 },
    { 0x11D8A, 0x11D8E },
    { 0x11D93, 0x11D94 },
    { 0x11D96, 0x11D96 },
    { 0x11EF5, 0x11EF6 },
    { 0x11F03, 0x11F03 },
    { 0x11F34, 0x11F35 },
    { 0x11F3E, 0x11F3F },
    { 0x1612A, 0x1612C },
    { 0x16F51, 0x16F87 },
};
static const size_t plum_term_spacing_mark_len = sizeof(plum_term_spacing_mark) / sizeof(plum_term_spacing_mark[0]);

static const struct plum_term_interval plum_term_prepend[] = {
    { 0x600, 0x605 },
    { 0x6DD, 0x6DD },
    { 0x70F, 0x70F },
    { 0x890, 0x891 },
    { 0x8E2, 0x8E2 },
    { 0xD4E, 0xD4E },
    { 0x110BD, 0x110BD },
    { 0x110CD, 0x110CD },
    { 0x111C2, 0x111C3 },
    { 0x113D1, 0x113D1 },
    { 0x1193F, 0x1193F },
    { 0x11941, 0x11941 },
    { 0x11A3A, 0x11A3A },
    { 0x11A84, 0x11A89 },
    { 0x11D46, 0x11D46 },
    { 0x11F02, 0x11F02 },
};
static const size_t plum_term_prepend_len = sizeof(plum_term_prepend) / sizeof(plum_term_prepend[0]);

static const struct plum_term_interval plum_term_control[] = {
    { 0x0, 0x9 },
    { 0xB, 0xC },
    { 0xE, 0x1F },
    { 0x7F, 0x9F },
    { 0xAD, 0xAD },
    { 0x61C, 0x61C },
    { 0x180E, 0x180E },
    { 0x200B, 0x200B },
    { 0x200E, 0x200F },
    { 0x2028, 0x202E },
    { 0x2060, 0x206F },
    { 0xFEFF, 0xFEFF },
    { 0xFFF0, 0xFFFB },
    { 0x13430, 0x1343F },
    { 0x1BCA0, 0x1BCA3 },
    { 0x1D173, 0x1D17A },
    { 0xE0000, 0xE001F },
    { 0xE0080, 0xE00FF },
    { 0xE01F0, 0xE0FFF },
};
static const size_t plum_term_control_len = sizeof(plum_term_control) / sizeof(plum_term_control[0]);

static const struct plum_term_interval plum_term_extended_pictographic[] = {
    { 0xA9, 0xA9 },
    { 0xAE, 0xAE },
    { 0x203C, 0x203C },
    { 0x2049, 0x2049 },
    { 0x2122, 0x2122 },
    { 0x2139, 0x2139 },
    { 0x2194, 0x2199 },
    { 0x21A9, 0x21AA },
    { 0x231A, 0x231B },
    { 0x2328, 0x2328 },
    { 0x2388, 0x2388 },
    { 0x23CF, 0x23CF },
    { 0x23E9, 0x23F3 },
    { 0x23F8, 0x23FA },
    { 0x24C2, 0x24C2 },
    { 0x25AA, 0x25AB },
    { 0x25B6, 0x25B6 },
    { 0x25C0, 0x25C0 },
    { 0x25FB, 0x25FE },
    { 0x2600, 0x2605 },
    { 0x2607, 0x2612 },
    { 0x2614, 0x2685 },
    { 0x2690, 0x2705 },
    { 0x2708, 0x2712 },
    { 0x2714, 0x2714 },
    { 0x2716, 0x2716 },
    { 0x271D, 0x271D },
    { 0x2721, 0x2721 },
    { 0x2728, 0x2728 },
    { 0x2733, 0x2734 },
    { 0x2744, 0x2744 },
    { 0x2747, 0x2747 },
    { 0x274C, 0x274C },
    { 0x274E, 0x274E },
    { 0x2753, 0x2755 },
    { 0x2757, 0x2757 },
    { 0x2763, 0x2767 },
    { 0x2795, 0x2797 },
    { 0x27A1, 0x27A1 },
    { 0x27B0, 0x27B0 },
    { 0x27BF, 0x27BF },
    { 0x2934, 0x2935 },
    { 0x2B05, 0x2B07 },
    { 0x2B1B, 0x2B1C },
    { 0x2B50, 0x2B50 },
    { 0x2B55, 0x2B55 },
    { 0x3030, 0x3030 },
    { 0x303D, 0x303D },
    { 0x3297, 0x3297 },
    { 0x3299, 0x3299 },
    { 0x1F000, 0x1F0FF },
    { 0x1F10D, 0x1F10F },
    { 0x1F12F, 0x1F12F },
    { 0x1F16C, 0x1F171 },
    { 0x1F17E, 0x1F17F },
    { 0x1F18E, 0x1F18E },
    { 0x1F191, 0x1F19A },
    { 0x1F1AD, 0x1F1E5 },
    { 0x1F201, 0x1F20F },
    { 0x1F21A, 0x1F21A },
    { 0x1F22F, 0x1F22F },
    { 0x1F232, 0x1F23A },
    { 0x1F23C, 0x1F23F },
    { 0x1F249, 0x1F3FA },
    { 0x1F400, 0x1F53D },
    { 0x1F546, 0x1F64F },
    { 0x1F680, 0x1F6FF },
    { 0x1F774, 0x1F77F },
    { 0x1F7D5, 0x1F7FF },
    { 0x1F80C, 0x1F80F },
    { 0x1F848, 0x1F84F },
    { 0x1F85A, 0x1F85F },
    { 0x1F888, 0x1F88F },
    { 0x1F8AE, 0x1F8FF },
    { 0x1F90C, 0x1F93A },
    { 0x1F93C, 0x1F945 },
    { 0x1F947, 0x1FAFF },
    { 0x1FC00, 0x1FFFD },
};
static const size_t plum_term_extended_pictographic_len = sizeof(plum_term_extended_pictographic) / sizeof(plum_term_extended_pictographic[0]);

#if defined(_WIN32)
#include <windows.h>
#include <io.h>
#else
#include <unistd.h>
#include <sys/ioctl.h>
#include <termios.h>
#endif

static long long plum_term_cols = 0;
static long long plum_term_rows = 0;

// Unicode's codepoint properties are fixed data, not a question for the
// host C library. `wcwidth` follows the locale and system Unicode version,
// which would make a prebuilt Plum program lay out the same text differently
// on two machines. These tables were generated from Unicode 16.0.0.
static int plum_term_in(const struct plum_term_interval *xs, size_t n, uint32_t cp) {
    for (size_t i = 0; i < n; i++) {
        if (cp >= xs[i].first && cp <= xs[i].last) return 1;
    }
    return 0;
}

static uint32_t plum_term_utf8(const unsigned char *s, size_t *used) {
    uint32_t a = s[0];
    if (a < 0x80) { *used = 1; return a; }
    if ((a & 0xe0) == 0xc0) { *used = 2; return ((a & 0x1f) << 6) | (s[1] & 0x3f); }
    if ((a & 0xf0) == 0xe0) { *used = 3; return ((a & 0x0f) << 12) | ((s[1] & 0x3f) << 6) | (s[2] & 0x3f); }
    *used = 4;
    return ((a & 0x07) << 18) | ((s[1] & 0x3f) << 12) | ((s[2] & 0x3f) << 6) | (s[3] & 0x3f);
}

enum plum_term_gcb {
    PLUM_GCB_OTHER, PLUM_GCB_CR, PLUM_GCB_LF, PLUM_GCB_CONTROL,
    PLUM_GCB_EXTEND, PLUM_GCB_ZWJ, PLUM_GCB_RI, PLUM_GCB_PREPEND,
    PLUM_GCB_SPACING, PLUM_GCB_L, PLUM_GCB_V, PLUM_GCB_T,
    PLUM_GCB_LV, PLUM_GCB_LVT, PLUM_GCB_EP
};

static enum plum_term_gcb plum_term_gcb(uint32_t cp) {
    if (cp == 0x0d) return PLUM_GCB_CR;
    if (cp == 0x0a) return PLUM_GCB_LF;
    if (plum_term_in(plum_term_control, plum_term_control_len, cp)) return PLUM_GCB_CONTROL;
    if (cp == 0x200d) return PLUM_GCB_ZWJ;
    if (cp >= 0x1f1e6 && cp <= 0x1f1ff) return PLUM_GCB_RI;
    if (plum_term_in(plum_term_extend, plum_term_extend_len, cp)) return PLUM_GCB_EXTEND;
    if (plum_term_in(plum_term_prepend, plum_term_prepend_len, cp)) return PLUM_GCB_PREPEND;
    if (plum_term_in(plum_term_spacing_mark, plum_term_spacing_mark_len, cp)) return PLUM_GCB_SPACING;
    if (cp >= 0x1100 && cp <= 0x115f) return PLUM_GCB_L;
    if (cp >= 0xa960 && cp <= 0xa97c) return PLUM_GCB_L;
    if ((cp >= 0x1160 && cp <= 0x11a7) || (cp >= 0xd7b0 && cp <= 0xd7c6)) return PLUM_GCB_V;
    if ((cp >= 0x11a8 && cp <= 0x11ff) || (cp >= 0xd7cb && cp <= 0xd7fb)) return PLUM_GCB_T;
    if (cp >= 0xac00 && cp <= 0xd7a3) return ((cp - 0xac00) % 28) ? PLUM_GCB_LVT : PLUM_GCB_LV;
    if (plum_term_in(plum_term_extended_pictographic, plum_term_extended_pictographic_len, cp)) return PLUM_GCB_EP;
    return PLUM_GCB_OTHER;
}

static int plum_term_char_width(uint32_t cp, enum plum_term_gcb kind) {
    if (kind == PLUM_GCB_CR || kind == PLUM_GCB_LF || kind == PLUM_GCB_CONTROL ||
        kind == PLUM_GCB_EXTEND || kind == PLUM_GCB_ZWJ || kind == PLUM_GCB_PREPEND ||
        kind == PLUM_GCB_SPACING || cp == 0x200c) return 0;
    return plum_term_in(plum_term_wide, plum_term_wide_len, cp) ? 2 : 1;
}

static int plum_term_breaks(enum plum_term_gcb prev, enum plum_term_gcb next,
                            int ep_before_zwj, int ri_count) {
    if (prev == PLUM_GCB_CR && next == PLUM_GCB_LF) return 0;
    if (prev == PLUM_GCB_CR || prev == PLUM_GCB_LF || prev == PLUM_GCB_CONTROL ||
        next == PLUM_GCB_CR || next == PLUM_GCB_LF || next == PLUM_GCB_CONTROL) return 1;
    if (prev == PLUM_GCB_L && (next == PLUM_GCB_L || next == PLUM_GCB_V || next == PLUM_GCB_LV || next == PLUM_GCB_LVT)) return 0;
    if ((prev == PLUM_GCB_LV || prev == PLUM_GCB_V) && (next == PLUM_GCB_V || next == PLUM_GCB_T)) return 0;
    if ((prev == PLUM_GCB_LVT || prev == PLUM_GCB_T) && next == PLUM_GCB_T) return 0;
    if (next == PLUM_GCB_EXTEND || next == PLUM_GCB_ZWJ || next == PLUM_GCB_SPACING) return 0;
    if (prev == PLUM_GCB_PREPEND) return 0;
    if (ep_before_zwj && next == PLUM_GCB_EP) return 0;
    if (prev == PLUM_GCB_RI && next == PLUM_GCB_RI && (ri_count % 2) == 1) return 0;
    return 1;
}

// Skips one ECMA-48 escape sequence. Escape bytes do not occupy cells.
static size_t plum_term_skip_escape(const unsigned char *s, size_t n, size_t i) {
    if (i + 1 >= n || s[i] != 0x1b) return i;
    if (s[i + 1] == '[') {
        i += 2;
        while (i < n && (s[i] < 0x40 || s[i] > 0x7e)) i++;
        return i < n ? i + 1 : n;
    }
    if (s[i + 1] == ']') {
        i += 2;
        while (i < n && s[i] != 0x07 && !(s[i] == 0x1b && i + 1 < n && s[i + 1] == '\\')) i++;
        return i < n && s[i] == 0x1b ? i + 2 : (i < n ? i + 1 : n);
    }
    return i + 2;
}

// Returns the display-cell width of UTF-8 text. It applies the UAX #29 rules
// that matter to terminal text -- combining marks, Hangul, emoji ZWJ
// sequences, and flags -- and gives each cluster its widest visible scalar.
// Thus an emoji ZWJ sequence and a flag remain two cells rather than being
// counted component by component.
long long term_display_width(const char *text) {
    const unsigned char *s = (const unsigned char *)text;
    if (!s) return 0;
    size_t n = strlen(text), i = 0, used;
    long long total = 0;
    int have = 0, cluster = 0, ri_count = 0, ep_seen = 0, ep_before_zwj = 0;
    enum plum_term_gcb prev = PLUM_GCB_OTHER;
    while (i < n) {
        if (s[i] == 0x1b) { i = plum_term_skip_escape(s, n, i); continue; }
        uint32_t cp = plum_term_utf8(s + i, &used);
        enum plum_term_gcb kind = plum_term_gcb(cp);
        int is_break = have && plum_term_breaks(prev, kind, ep_before_zwj, ri_count);
        if (is_break) { total += cluster; cluster = 0; ri_count = 0; ep_seen = 0; ep_before_zwj = 0; }
        int width = plum_term_char_width(cp, kind);
        if (width > cluster) cluster = width;
        if (cp == 0xfe0f && cluster < 2) cluster = 2;
        if (kind == PLUM_GCB_RI) ri_count++; else if (kind != PLUM_GCB_EXTEND) ri_count = 0;
        if (kind == PLUM_GCB_EP) { ep_seen = 1; ep_before_zwj = 0; }
        else if (kind == PLUM_GCB_EXTEND && ep_seen) { }
        else if (kind == PLUM_GCB_ZWJ) { ep_before_zwj = ep_seen; ep_seen = 0; }
        else { ep_seen = 0; ep_before_zwj = 0; }
        prev = kind; have = 1; i += used;
    }
    return total + (have ? cluster : 0);
}

// Byte offset after the longest whole grapheme cluster that fits in `cells`.
// A Plum String may slice at this offset safely: it is always a UTF-8 and
// grapheme boundary. ANSI escapes are carried through but never consume a
// cell; an escape immediately before an omitted cluster is omitted too.
long long term_truncate_bytes(const char *text, long long cells) {
    const unsigned char *s = (const unsigned char *)text;
    if (!s || cells <= 0) return 0;
    size_t n = strlen(text), i = 0, used, accepted = 0;
    long long total = 0;
    int have = 0, cluster = 0, ri_count = 0, ep_seen = 0, ep_before_zwj = 0;
    enum plum_term_gcb prev = PLUM_GCB_OTHER;
    while (i < n) {
        if (s[i] == 0x1b) { i = plum_term_skip_escape(s, n, i); continue; }
        uint32_t cp = plum_term_utf8(s + i, &used);
        enum plum_term_gcb kind = plum_term_gcb(cp);
        int is_break = have && plum_term_breaks(prev, kind, ep_before_zwj, ri_count);
        if (is_break) {
            if (total + cluster > cells) return (long long)accepted;
            total += cluster; accepted = i;
            cluster = 0; ri_count = 0; ep_seen = 0; ep_before_zwj = 0;
        }
        int width = plum_term_char_width(cp, kind);
        if (width > cluster) cluster = width;
        if (cp == 0xfe0f && cluster < 2) cluster = 2;
        if (kind == PLUM_GCB_RI) ri_count++; else if (kind != PLUM_GCB_EXTEND) ri_count = 0;
        if (kind == PLUM_GCB_EP) { ep_seen = 1; ep_before_zwj = 0; }
        else if (kind == PLUM_GCB_EXTEND && ep_seen) { }
        else if (kind == PLUM_GCB_ZWJ) { ep_before_zwj = ep_seen; ep_seen = 0; }
        else { ep_seen = 0; ep_before_zwj = 0; }
        prev = kind; have = 1; i += used;
    }
    return total + cluster <= cells ? (long long)n : (long long)accepted;
}

// 0 stdin, 1 stdout, 2 stderr -- the numbers Plum's `Stream` enum maps
// to. Anything else is not a terminal, which is the safe answer.
long long term_is_tty(long long which) {
    if (which < 0 || which > 2) return 0;
#if defined(_WIN32)
    return _isatty((int)which) ? 1 : 0;
#else
    return isatty((int)which) ? 1 : 0;
#endif
}

// Asks the terminal how big it is, storing the answer for the two
// readers below. Returns 0 on success and -1 when there is no terminal
// to ask -- output redirected to a file, for instance, which is not an
// error the caller did anything wrong to cause.
//
// Measured on STDOUT, not stdin: the size that matters is the one of
// the thing being drawn to. A program whose stdin is a pipe and whose
// stdout is a terminal still has a size worth knowing.
long long term_size_query(void) {
#if defined(_WIN32)
    CONSOLE_SCREEN_BUFFER_INFO info;
    HANDLE h = GetStdHandle(STD_OUTPUT_HANDLE);
    if (h == INVALID_HANDLE_VALUE || h == NULL) return -1;
    if (!GetConsoleScreenBufferInfo(h, &info)) return -1;
    // The WINDOW, not the buffer. A Windows console's buffer is
    // routinely taller than the window -- that is what the scrollback
    // is -- so `dwSize` would report a height nobody can see.
    plum_term_cols = (long long)(info.srWindow.Right - info.srWindow.Left + 1);
    plum_term_rows = (long long)(info.srWindow.Bottom - info.srWindow.Top + 1);
#else
    struct winsize ws;
    if (ioctl(STDOUT_FILENO, TIOCGWINSZ, &ws) != 0) return -1;
    plum_term_cols = (long long)ws.ws_col;
    plum_term_rows = (long long)ws.ws_row;
#endif
    // A terminal reporting zero is a terminal that does not know, and a
    // caller dividing by it would be worse off than one told no.
    if (plum_term_cols <= 0 || plum_term_rows <= 0) return -1;
    return 0;
}

long long term_size_cols(void) { return plum_term_cols; }
long long term_size_rows(void) { return plum_term_rows; }

// Writes to stdout with NO trailing newline and NO flush.
//
// Both omissions are the point. A terminal program positions its own
// output with escape sequences, so an automatic newline would corrupt
// every frame it draws; and flushing per write turns one redraw into
// hundreds of syscalls. `term_flush` is separate so a caller can build
// a frame and publish it in one go.
//
// Returns -1 if the write failed -- a closed pipe, most likely, which a
// long-running program should notice rather than draw into forever.
long long term_write(const char *s) {
    if (s == NULL) return 0;
    if (fputs(s, stdout) == EOF) return -1;
    return 0;
}

long long term_flush(void) { return fflush(stdout) == 0 ? 0 : -1; }

// --- Raw mode, the alternate screen, and the cursor ---
//
// Each of these is entered by a call that SAVES what it is about to
// change, and left by one that puts it back. On the Plum side each is a
// `handle`, so leaving happens when the value dies -- including on a
// panic, which is what issue #1 added and what a terminal program needs
// more than most: a crash that leaves a terminal in raw mode with no
// cursor is a crash that also takes the user's shell with it.
//
// --- Why entering is COUNTED rather than refused ---
//
// Entering twice must not save the already-changed state as the thing
// to restore, or leaving puts the terminal back to raw. The first entry
// saves and applies; later ones only count; the last leave restores.
// Counting rather than refusing means a library and its caller can both
// ask without knowing about each other, and it is order-independent,
// which matters because handles in different scopes need not die in the
// order they were born.

static int plum_raw_depth = 0;
static int plum_alt_depth = 0;
static int plum_cursor_depth = 0;

#if !defined(_WIN32)
static struct termios plum_saved_termios;
#else
static DWORD plum_saved_in_mode = 0;
static DWORD plum_saved_out_mode = 0;
#endif

// Returns 0 on success, -1 if there is no terminal to change.
//
// **`ISIG` is cleared, so Ctrl+C arrives as a BYTE (0x03) rather than a
// signal.** That is what raw mode means, and here it is also what makes
// cleanup work: the default action for SIGINT terminates the process
// WITHOUT running `atexit` handlers, so a Ctrl+C in raw mode with
// signals still enabled would leave the terminal raw and the cursor
// hidden. Delivered as a byte, the program can exit through `exit()`,
// which restores everything on the way out. A raw-mode program is
// responsible for noticing 0x03 and quitting.
//
// `VMIN=1, VTIME=0`: a read blocks until at least one byte. The timeout
// belongs to `poll`, which is what `Os.read_stdin_timeout` already
// uses; asking termios for a timeout as well would give two mechanisms
// racing over one deadline.
long long term_enter_raw(void) {
#if defined(_WIN32)
    HANDLE h = GetStdHandle(STD_INPUT_HANDLE);
    if (h == INVALID_HANDLE_VALUE || h == NULL) return -1;
    if (plum_raw_depth == 0) {
        if (!GetConsoleMode(h, &plum_saved_in_mode)) return -1;
        DWORD mode = plum_saved_in_mode;
        mode &= ~(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
        // The reason one decoder can serve every platform: with this
        // set, the console delivers the same VT escape sequences a
        // POSIX terminal does, instead of INPUT_RECORD structures.
        mode |= ENABLE_VIRTUAL_TERMINAL_INPUT;
        if (!SetConsoleMode(h, mode)) return -1;
    }
    plum_raw_depth++;
    return 0;
#else
    if (!isatty(STDIN_FILENO)) return -1;
    if (plum_raw_depth == 0) {
        if (tcgetattr(STDIN_FILENO, &plum_saved_termios) != 0) return -1;
        struct termios raw = plum_saved_termios;
        raw.c_iflag &= (tcflag_t)~(IXON | ICRNL | BRKINT | INPCK | ISTRIP);
        raw.c_oflag &= (tcflag_t)~(OPOST);
        raw.c_lflag &= (tcflag_t)~(ECHO | ICANON | IEXTEN | ISIG);
        raw.c_cflag |= (tcflag_t)CS8;
        raw.c_cc[VMIN] = 1;
        raw.c_cc[VTIME] = 0;
        if (tcsetattr(STDIN_FILENO, TCSAFLUSH, &raw) != 0) return -1;
    }
    plum_raw_depth++;
    return 0;
#endif
}

// Takes no argument it uses: a `handle`'s cleanup is called with the
// number the cell holds, and these have no per-instance state -- the
// depth counter is what decides whether this one is the last.
void term_leave_raw(long long ignored) {
    (void)ignored;
    if (plum_raw_depth <= 0) return;
    plum_raw_depth--;
    if (plum_raw_depth > 0) return;
#if defined(_WIN32)
    HANDLE h = GetStdHandle(STD_INPUT_HANDLE);
    if (h != INVALID_HANDLE_VALUE && h != NULL) SetConsoleMode(h, plum_saved_in_mode);
#else
    tcsetattr(STDIN_FILENO, TCSAFLUSH, &plum_saved_termios);
#endif
}

// Windows needs telling that escape sequences written to stdout are
// escape sequences. POSIX terminals need no equivalent.
#if defined(_WIN32)
static int plum_enable_vt_output(void) {
    HANDLE h = GetStdHandle(STD_OUTPUT_HANDLE);
    if (h == INVALID_HANDLE_VALUE || h == NULL) return -1;
    if (!GetConsoleMode(h, &plum_saved_out_mode)) return -1;
    return SetConsoleMode(h, plum_saved_out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) ? 0 : -1;
}
#endif

// `\033[?1049h` and `?1049l`: the xterm alternate screen. Entering
// leaves the user's scrollback untouched and leaving puts their shell
// back exactly as it was, which is why a full-screen program should use
// it rather than clearing.
//
// Flushed immediately, unlike `term_write`. A mode change that has not
// reached the terminal yet is a mode change that has not happened, and
// everything drawn after it would land on the wrong screen.
long long term_enter_alt(void) {
    // Refused when stdout is not a terminal. Escape sequences written
    // into a file or a pipe are not invisible -- they are corruption,
    // and the caller gets an `Err` they can act on instead.
    if (!term_is_tty(1)) return -1;
    if (plum_alt_depth == 0) {
#if defined(_WIN32)
        if (plum_enable_vt_output() != 0) return -1;
#endif
        if (fputs("\033[?1049h", stdout) == EOF) return -1;
        if (fflush(stdout) != 0) return -1;
    }
    plum_alt_depth++;
    return 0;
}

void term_leave_alt(long long ignored) {
    (void)ignored;
    if (plum_alt_depth <= 0) return;
    plum_alt_depth--;
    if (plum_alt_depth > 0) return;
    fputs("\033[?1049l", stdout);
    fflush(stdout);
}

long long term_hide_cursor(void) {
    if (!term_is_tty(1)) return -1;
    if (plum_cursor_depth == 0) {
#if defined(_WIN32)
        if (plum_enable_vt_output() != 0) return -1;
#endif
        if (fputs("\033[?25l", stdout) == EOF) return -1;
        if (fflush(stdout) != 0) return -1;
    }
    plum_cursor_depth++;
    return 0;
}

void term_show_cursor(long long ignored) {
    (void)ignored;
    if (plum_cursor_depth <= 0) return;
    plum_cursor_depth--;
    if (plum_cursor_depth > 0) return;
    fputs("\033[?25h", stdout);
    fflush(stdout);
}
