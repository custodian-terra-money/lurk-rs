use super::constraints::{alloc_is_zero, equal, pick, select};
use bellperson::{
    bls::Engine,
    gadgets::boolean::{AllocatedBit, Boolean},
    gadgets::num::AllocatedNum,
    ConstraintSystem, LinearCombination, SynthesisError,
};
use ff::Field;

pub struct CaseClause<E: Engine> {
    pub key: E::Fr,
    pub value: AllocatedNum<E>,
}

pub struct CaseConstraint<'a, E: Engine> {
    selected: AllocatedNum<E>,
    clauses: &'a [CaseClause<E>],
}

impl<E: Engine> CaseConstraint<'_, E> {
    fn enforce_selection<CS: ConstraintSystem<E>>(
        self,
        cs: &mut CS,
    ) -> Result<AllocatedNum<E>, SynthesisError> {
        // Allocate one bit per clause, the selector. This creates constraints enforcing that each bit is 0 or 1.
        // In fact, the 'selected' clause will have selector = 1 while the others = 0.
        // This will be confirmed/enforced by later constraints.
        let mut selectors = Vec::with_capacity(self.clauses.len());
        for (i, clause) in self.clauses.iter().enumerate() {
            let is_selected = if let Some(value) = self.selected.get_value() {
                clause.key == value
            } else {
                false
            };
            selectors.push(
                AllocatedBit::alloc(
                    cs.namespace(|| format!("selector {}", i)),
                    Some(is_selected),
                )
                .unwrap(),
            );
        }

        cs.enforce(
            || "exactly one selector is 1",
            |lc| {
                selectors
                    .iter()
                    .fold(lc, |lc, selector| lc + selector.get_variable())
            },
            |lc| lc + CS::one(),
            |lc| lc + CS::one(),
        );

        cs.enforce(
            || "selection vector dot keys = selected",
            |lc| {
                selectors
                    .iter()
                    .zip(&*self.clauses)
                    .fold(lc, |lc, (selector, clause)| {
                        lc + (clause.key, selector.get_variable())
                    })
            },
            |lc| lc + CS::one(),
            |lc| lc + self.selected.get_variable(),
        );

        let values = self
            .clauses
            .iter()
            .map(|c| c.value.clone())
            .collect::<Vec<_>>();

        let result = bit_dot_product(&mut cs.namespace(|| "extract result"), &selectors, &values)?;

        Ok(result)
    }
}

fn bit_dot_product<E: Engine, CS: ConstraintSystem<E>>(
    cs: &mut CS,
    bit_vector: &[AllocatedBit],
    value_vector: &[AllocatedNum<E>],
) -> Result<AllocatedNum<E>, SynthesisError> {
    let mut computed_result = E::Fr::zero();

    let mut all_products = Vec::new();

    for (i, (bit, value)) in bit_vector.iter().zip(value_vector).enumerate() {
        let prod = if bit.get_value().unwrap() {
            value.get_value().unwrap()
        } else {
            E::Fr::zero()
        };

        let allocated_prod =
            AllocatedNum::<E>::alloc(&mut cs.namespace(|| format!("product-{}", i)), || Ok(prod))?;

        cs.enforce(
            || format!("bit product {}", i),
            |lc| lc + bit.get_variable(),
            |lc| lc + value.get_variable(),
            |lc| lc + allocated_prod.get_variable(),
        );

        all_products.push(allocated_prod);
        computed_result.add_assign(&prod);
    }

    let result = AllocatedNum::<E>::alloc(&mut cs.namespace(|| "result"), || Ok(computed_result))?;

    cs.enforce(
        || "sum of products",
        |lc| {
            all_products
                .iter()
                .fold(lc, |acc, prod| acc + prod.get_variable())
        },
        |lc| lc + CS::one(),
        |lc| lc + result.get_variable(),
    );

    Ok(result)
}

pub fn case<E: Engine, CS: ConstraintSystem<E>>(
    cs: &mut CS,
    selected: &AllocatedNum<E>,
    clauses: &[CaseClause<E>],
    default: &AllocatedNum<E>,
) -> Result<AllocatedNum<E>, SynthesisError> {
    assert!(clauses.len() > 0);

    let mut maybe_selected = None;

    let mut acc = AllocatedNum::alloc(cs.namespace(|| "acc"), || Ok(E::Fr::one()))?;

    for (i, clause) in clauses.iter().enumerate() {
        if Some(clause.key) == selected.get_value() {
            maybe_selected = Some(selected.clone());
        }

        let mut x = clause.key;
        x.sub_assign(&selected.get_value().unwrap());
        x.mul_assign(&acc.get_value().unwrap());

        let new_acc = AllocatedNum::alloc(cs.namespace(|| format!("acc {})", i + 1)), || Ok(x))?;

        // acc * clause.key - selected = new_acc
        cs.enforce(
            || format!("acc * (clause-{}.key - selected) = new_acc", i),
            |lc| lc + acc.get_variable(),
            |_| Boolean::Constant(true).lc(CS::one(), clause.key) - selected.get_variable(),
            |lc| lc + new_acc.get_variable(),
        );

        acc = new_acc;
    }
    let is_selected = alloc_is_zero(cs.namespace(|| "is_selected"), &acc)?;
    // If no selection matched, use a dummy key so constraints are met.
    // We will actually return the default value, though.
    let dummy_key = AllocatedNum::alloc(cs.namespace(|| "default key"), || Ok(clauses[0].key))?;
    let selected = maybe_selected.unwrap_or(dummy_key);

    // TODO: Ensure cases contain no duplicate keys.
    let cc = CaseConstraint {
        selected: selected.clone(),
        clauses,
    };

    // If no selection matched, choose the default value.
    let is_default = is_selected.not();

    let enforced_result = cc.enforce_selection(cs)?;

    pick(
        &mut cs.namespace(|| "maybe default"),
        &is_default,
        &default,
        &enforced_result,
    )
}

// TODO: This can be optimized to minimize work duplicated between the inner case calls.
pub fn multi_case<E: Engine, CS: ConstraintSystem<E>>(
    cs: &mut CS,
    selected: &AllocatedNum<E>,
    cases: &[&[CaseClause<E>]],
    defaults: &[AllocatedNum<E>],
) -> Result<Vec<AllocatedNum<E>>, SynthesisError> {
    let mut result = Vec::new();

    for (i, (c, default)) in cases.iter().zip(defaults).enumerate() {
        result.push(case(
            &mut cs.namespace(|| format!("case {}", i)),
            selected,
            c,
            default,
        )?);
    }
    Ok(result)
}
mod tests {
    use super::*;
    use bellperson::bls::{Bls12, Fr, FrRepr};
    use bellperson::util_cs::test_cs::TestConstraintSystem;
    use ff::PrimeField;

    #[test]
    fn simple_case() {
        let mut cs = TestConstraintSystem::<Bls12>::new();

        let x = Fr::from_repr(FrRepr::from(123)).unwrap();
        let y = Fr::from_repr(FrRepr::from(124)).unwrap();
        let selected = AllocatedNum::alloc(cs.namespace(|| "selected"), || Ok(x)).unwrap();
        let val = AllocatedNum::alloc(cs.namespace(|| "val"), || {
            Ok(Fr::from_repr(FrRepr::from(666)).unwrap())
        })
        .unwrap();
        let val2 = AllocatedNum::alloc(cs.namespace(|| "val2"), || {
            Ok(Fr::from_repr(FrRepr::from(777)).unwrap())
        })
        .unwrap();
        let default = AllocatedNum::alloc(cs.namespace(|| "default"), || {
            Ok(Fr::from_repr(FrRepr::from(999)).unwrap())
        })
        .unwrap();

        {
            let clauses = [
                CaseClause {
                    key: x,
                    value: val.clone(),
                },
                CaseClause {
                    key: y,
                    value: val2.clone(),
                },
            ];

            let result = case(
                &mut cs.namespace(|| "selected case"),
                &selected,
                &clauses,
                &default,
            )
            .unwrap();

            assert_eq!(val.get_value(), result.get_value());
            assert!(cs.is_satisfied());
        }

        {
            let clauses = [CaseClause {
                key: y,
                value: val.clone(),
            }];

            let result = case(
                &mut cs.namespace(|| "default case"),
                &selected,
                &clauses,
                &default,
            )
            .unwrap();

            assert_eq!(default.get_value(), result.get_value());
            assert!(cs.is_satisfied());
        }
    }
}