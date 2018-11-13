// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Authority params deserialization.

use uint::Uint;
use ethereum_types::Address;


#[derive(Debug, PartialEq, Deserialize)]
pub struct TLEngineValidatorWeight {
	pub address: Address,
	pub weight: f64,
}

/// Authority params deserialization.
#[derive(Debug, PartialEq, Deserialize)]
pub struct TLEngineParams {
	pub fault_tolerance_thr: Option<f64>,
	pub validators: Vec<TLEngineValidatorWeight>,
}

/// Authority engine deserialization.
#[derive(Debug, PartialEq, Deserialize)]
pub struct TLEngine {
	/// Ethash params.
	pub params: TLEngineParams,
}

#[cfg(test)]
mod tests {
	use uint::Uint;
	use serde_json;
	use spec::tl_engine::TLEngine;
	use super::TLEngineValidatorWeight;
	use ethereum_types::Address;

	#[test]
	fn tl_params_deserialization() {
		let s = r#"{
			"params": {
				"fault_tolerance_thr": 10.0,
				"validators": [
					{ "address": "0xc6d9d2cd449a754c494264e1809c50e34d64562b", "weight": 1.2 },
					{ "address": "0xd6d9d2cd449a754c494264e1809c50e34d64562b", "weight": 0.2 }
				]
			}
		}"#;

		let deserialized: TLEngine = serde_json::from_str(s).unwrap();
		assert_eq!(deserialized.params.fault_tolerance_thr, Some(10.0.into()));

		let len = deserialized.params.validators.len();
		assert_eq!(len, 2);

		let validator: &TLEngineValidatorWeight = &deserialized.params.validators[0];

		assert_eq!(Address::from("0xc6d9d2cd449a754c494264e1809c50e34d64562b"), validator.address);
		assert_eq!(1.2, validator.weight);

		let validator = &deserialized.params.validators[1];
		assert_eq!(Address::from("0xd6d9d2cd449a754c494264e1809c50e34d64562b"), validator.address);
		assert_eq!(0.2, validator.weight);
	}
}
