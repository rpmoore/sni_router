// Copyright 2026 Ryan Moore
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

/// A plaintext TLS fatal `unrecognized_name` alert (RFC 6066 §3): record
/// type 21 (alert), legacy version 3.3, length 2, level 2 (fatal),
/// description 112. Sent when the SNI has no route or is missing, so clients
/// see a TLS error instead of a bare connection reset.
pub const UNRECOGNIZED_NAME_ALERT: [u8; 7] = [0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x70];
