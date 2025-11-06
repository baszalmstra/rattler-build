use marked_yaml::Span;

use crate::{
    _partialerror,
    recipe::{
        Jinja,
        custom_yaml::{HasSpan, RenderedNode, RenderedSequenceNode, TryConvertNode},
        error::{ErrorKind, PartialParsingError},
    },
};

#[derive(Default, Debug, Clone)]
pub struct Skip(Vec<(String, Span)>, Option<bool>);

impl TryConvertNode<Vec<(String, Span)>> for RenderedSequenceNode {
    fn try_convert(&self, name: &str) -> Result<Vec<(String, Span)>, Vec<PartialParsingError>> {
        let mut conditions = vec![];

        for node in self.iter() {
            match node {
                RenderedNode::Scalar(scalar) => {
                    let s: String = scalar.try_convert(name)?;
                    conditions.push((s, *node.span()))
                }
                _ => {
                    return Err(vec![_partialerror!(
                        *node.span(),
                        ErrorKind::ExpectedScalar,
                    )]);
                }
            }
        }
        Ok(conditions)
    }
}

impl TryConvertNode<Skip> for RenderedNode {
    fn try_convert(&self, name: &str) -> Result<Skip, Vec<PartialParsingError>> {
        let conditions = match self {
            RenderedNode::Scalar(scalar) => vec![(scalar.try_convert(name)?, *self.span())],
            RenderedNode::Sequence(sequence) => sequence.try_convert(name)?,
            RenderedNode::Mapping(_) => {
                return Err(vec![_partialerror!(
                    *self.span(),
                    ErrorKind::ExpectedSequence,
                )]);
            }
            RenderedNode::Null(_) => vec![],
        };

        Ok(Skip(conditions, None))
    }
}

impl Skip {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Merge two Skip instances by combining their conditions.
    /// If both are already evaluated, the evaluation results are merged using OR logic
    /// (if either is true, the merged result is true).
    /// Otherwise, the evaluation result is reset to None.
    pub fn merge(mut self, other: Skip) -> Self {
        // Add all conditions from the other Skip to this one
        self.0.extend(other.0);

        // Merge evaluation results if both are already evaluated
        let merged_eval = match (self.1, other.1) {
            // If either evaluation is true, the merged result is true (OR logic)
            (Some(true), _) | (_, Some(true)) => Some(true),
            // If both are false, the merged result is false
            (Some(false), Some(false)) => Some(false),
            // If either is not evaluated, we need to re-evaluate
            _ => None,
        };

        Skip(self.0, merged_eval)
    }

    pub fn with_eval(self, jinja: &Jinja) -> Result<Self, Vec<PartialParsingError>> {
        for condition in &self.0 {
            match jinja.eval(&condition.0) {
                Ok(res) => {
                    if res.is_true() {
                        return Ok(Skip(self.0, Some(true)));
                    }
                }
                Err(e) => {
                    return Err(vec![_partialerror!(
                        condition.1,
                        ErrorKind::JinjaRendering(Box::new(e)),
                    )]);
                }
            }
        }
        Ok(Skip(self.0, Some(false)))
    }

    pub fn eval(&self) -> bool {
        self.1.unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marked_yaml::Span;

    #[test]
    fn test_merge_both_unevaluated() {
        let skip1 = Skip(vec![("linux".to_string(), Span::new_blank())], None);
        let skip2 = Skip(vec![("osx".to_string(), Span::new_blank())], None);

        let merged = skip1.merge(skip2);

        // Should have both conditions
        assert_eq!(merged.0.len(), 2);
        assert_eq!(merged.0[0].0, "linux");
        assert_eq!(merged.0[1].0, "osx");
        // Should not be evaluated
        assert_eq!(merged.1, None);
    }

    #[test]
    fn test_merge_both_evaluated_false() {
        let skip1 = Skip(vec![("linux".to_string(), Span::new_blank())], Some(false));
        let skip2 = Skip(vec![("osx".to_string(), Span::new_blank())], Some(false));

        let merged = skip1.merge(skip2);

        // Should have both conditions
        assert_eq!(merged.0.len(), 2);
        // Should be evaluated to false (both were false)
        assert_eq!(merged.1, Some(false));
    }

    #[test]
    fn test_merge_one_true_one_false() {
        let skip1 = Skip(vec![("linux".to_string(), Span::new_blank())], Some(true));
        let skip2 = Skip(vec![("osx".to_string(), Span::new_blank())], Some(false));

        let merged = skip1.merge(skip2);

        // Should have both conditions
        assert_eq!(merged.0.len(), 2);
        // Should be evaluated to true (OR logic)
        assert_eq!(merged.1, Some(true));
    }

    #[test]
    fn test_merge_both_true() {
        let skip1 = Skip(vec![("linux".to_string(), Span::new_blank())], Some(true));
        let skip2 = Skip(vec![("osx".to_string(), Span::new_blank())], Some(true));

        let merged = skip1.merge(skip2);

        // Should have both conditions
        assert_eq!(merged.0.len(), 2);
        // Should be evaluated to true
        assert_eq!(merged.1, Some(true));
    }

    #[test]
    fn test_merge_one_evaluated_one_not() {
        let skip1 = Skip(vec![("linux".to_string(), Span::new_blank())], Some(false));
        let skip2 = Skip(vec![("osx".to_string(), Span::new_blank())], None);

        let merged = skip1.merge(skip2);

        // Should have both conditions
        assert_eq!(merged.0.len(), 2);
        // Should not be evaluated (one is None)
        assert_eq!(merged.1, None);
    }

    #[test]
    fn test_merge_true_with_unevaluated() {
        let skip1 = Skip(vec![("linux".to_string(), Span::new_blank())], Some(true));
        let skip2 = Skip(vec![("osx".to_string(), Span::new_blank())], None);

        let merged = skip1.merge(skip2);

        // Should have both conditions
        assert_eq!(merged.0.len(), 2);
        // Should be evaluated to true (one is true)
        assert_eq!(merged.1, Some(true));
    }
}
