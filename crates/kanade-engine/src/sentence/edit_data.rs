// SPDX-License-Identifier: MIT
// Rank and distance of two native replacements, ordered by their frequency
// in aligned segments of source-confirmed training conversions.
pub(super) const EDIT_PAIRS: &[(usize, usize, usize)] = &[
    (1, 1, 1),
    (3, 1, 1),
    (2, 1, 1),
    (1, 2, 1),
    (1, 1, 2),
    (1, 1, 3),
    (1, 3, 1),
];
