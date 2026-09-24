// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Standalone build of the in-script `selfci` client, for hosts that
//! don't embed it as a multi-call binary (and for this crate's tests).

fn main() {
    rho_ci::client::run_client(std::env::args().skip(1).collect());
}
