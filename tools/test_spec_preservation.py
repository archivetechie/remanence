"""Additional regression cases for disposition and review boundaries."""
import unittest
import spec_preservation as p


class DispositionTests(unittest.TestCase):
    old = '# Root\n\n## Rule\n\nReaders MUST preserve `field`.\n'
    new = '# Root\n\n## Rule\n\nReaders MUST retain `field`.\n'

    def evidence(self, status='changed'):
        inventory = p.inventory(self.old, self.new)
        dispositions = {k: inventory[k] for k in ('baseline_sha256', 'candidate_sha256')}
        dispositions['items'] = [{'id': i['id'], 'status': status, 'candidate_span': [5, 5],
                                  'rationale': 'The reviewer accepted this explicit semantic change.'} for i in inventory['items']]
        receipt = {**{k: inventory[k] for k in ('baseline_sha256', 'candidate_sha256')},
                   'inventory_sha256': p.canonical_hash(inventory), 'dispositions_sha256': p.canonical_hash(dispositions),
                   'author': 'author-session', 'reviewer': 'independent-session', 'verdict': 'passed',
                   'reviewed_ids': [i['id'] for i in inventory['items']]}
        return dispositions, receipt

    def cross_file_evidence(self, old, candidate, *, status='moved', target_file='spec/new.md',
                            target=None, candidate_files=None, exceptions=None):
        inventory = p.inventory(old, candidate)
        dispositions = {k: inventory[k] for k in ('baseline_sha256', 'candidate_sha256')}
        candidate_files = candidate_files or {target_file: target if target is not None else candidate}
        target = target if target is not None else candidate_files[target_file]
        dispositions['items'] = [
            {'id': item['id'], 'status': status, 'candidate_file': target_file,
             'candidate_span': [1, len(target.splitlines())],
             **({'exceptions': exceptions} if exceptions is not None else {}),
             **({'rationale': 'The reviewer accepted this explicit semantic change.'}
                if status in {'changed', 'retired'} else {})}
            for item in inventory['items']
        ]
        receipt = {**{k: inventory[k] for k in ('baseline_sha256', 'candidate_sha256')},
                   'inventory_sha256': p.canonical_hash(inventory),
                   'dispositions_sha256': p.canonical_hash(dispositions),
                   'candidate_files_sha256': p.canonical_hash(
                       {name: p.sha(text) for name, text in candidate_files.items()}),
                   'author': 'author-session', 'reviewer': 'independent-session',
                   'verdict': 'passed',
                   'reviewed_ids': [item['id'] for item in inventory['items']]}
        return dispositions, receipt, candidate_files

    def test_changed_content_needs_matching_independent_receipt(self):
        dispositions, receipt = self.evidence()
        self.assertEqual(p.check(self.old, self.new, dispositions, receipt), [])
        receipt['candidate_sha256'] = 'stale'
        self.assertTrue(p.check(self.old, self.new, dispositions, receipt))

    def test_missing_author_is_not_independent(self):
        dispositions, receipt = self.evidence()
        del receipt['author']
        self.assertTrue(p.check(self.old, self.new, dispositions, receipt))

    def test_retained_span_cannot_contain_extra_content(self):
        old, new = '| key | old |\n', '| key | old | extra |\n'
        inventory = p.inventory(old, new)
        dispositions = {k: inventory[k] for k in ('baseline_sha256', 'candidate_sha256')}
        dispositions['items'] = [{'id': i['id'], 'status': 'retained', 'candidate_span': [1, 1]} for i in inventory['items']]
        self.assertTrue(p.check(old, new, dispositions))

    def test_retirement_requires_real_design_and_substantive_classification(self):
        inventory = p.inventory(self.old, self.new)
        root, child = [s['id'] for s in inventory['sections']]
        dispositions, receipt = self.evidence('retired')
        for row in dispositions['items']:
            row['design_item'] = child
        design = {'baseline_sha256': p.sha(self.old), 'items': {child: 'retired'}}
        receipt.update(dispositions_sha256=p.canonical_hash(dispositions), design_sha256=p.canonical_hash(design))
        self.assertEqual(p.check(self.old, self.new, dispositions, receipt, design, edit_kind='substantive'), [])
        for mode in (None, 'wording'):
            self.assertTrue(p.check(self.old, self.new, dispositions, receipt, design, edit_kind=mode))
        design['items'][root] = 'retained'
        receipt['design_sha256'] = p.canonical_hash(design)
        self.assertTrue(p.check(self.old, self.new, dispositions, receipt, design, edit_kind='substantive'))
        for row in dispositions['items']:
            row['design_item'] = 'invented'
        design['items']['invented'] = 'retired'
        receipt.update(dispositions_sha256=p.canonical_hash(dispositions), design_sha256=p.canonical_hash(design))
        self.assertTrue(p.check(self.old, self.new, dispositions, receipt, design, edit_kind='substantive'))

    def test_cross_file_move_and_changed_content_use_candidate_file_spans(self):
        old = 'Readers MUST preserve `field` under RFC 9180.\n'
        target = 'Readers MUST retain `field` under RFC 9180.\n'
        dispositions, receipt, candidate_files = self.cross_file_evidence(
            old, '', status='changed', target=target,
            candidate_files={'spec/new.md': target, 'spec/other.md': 'Unrelated text.\n'})
        self.assertEqual(p.check(old, '', dispositions, receipt,
                                 candidate_files=candidate_files), [])

        moved_dispositions, moved_receipt, moved_files = self.cross_file_evidence(
            old, '', status='moved', target=old, candidate_files={'spec/new.md': old})
        self.assertEqual(p.check(old, '', moved_dispositions, moved_receipt,
                                 candidate_files=moved_files), [])

    def test_cross_file_disposition_rejects_missing_target(self):
        old = 'Readers MUST preserve `field`.\n'
        dispositions, receipt, candidate_files = self.cross_file_evidence(
            old, '', status='moved', target=old, candidate_files={'spec/other.md': old})
        dispositions['items'][0]['candidate_file'] = 'spec/missing.md'
        self.assertTrue(p.check(old, '', dispositions, receipt,
                                candidate_files=candidate_files))

    def test_changed_content_requires_identifier_and_rfc_minimums(self):
        cases = (
            ('Use `rem-object-v1` as specified by RFC 9180.\n',
             'Use the replacement identifier as specified by RFC 9180.\n'),
            ('Use the format specified by RFC 9180.\n',
             'Use the format specified by the project.\n'),
        )
        for old, target in cases:
            with self.subTest(old=old):
                dispositions, receipt, candidate_files = self.cross_file_evidence(
                    old, '', status='changed', target=target,
                    candidate_files={'spec/new.md': target})
                self.assertTrue(
                    p.check(old, '', dispositions, receipt, candidate_files=candidate_files),
                    target)

    def test_explicitly_reviewed_identifier_exception_is_bound_to_changed_item(self):
        old = 'Use `rem-object-v1` under RFC 9180.\n'
        target = 'Use the replacement under RFC 9180.\n'
        dispositions, receipt, candidate_files = self.cross_file_evidence(
            old, '', status='changed', target=target,
            candidate_files={'spec/new.md': target}, exceptions=['identifiers'])
        self.assertEqual(p.check(old, '', dispositions, receipt,
                                 candidate_files=candidate_files, edit_kind="substantive"), [])

    def test_changed_replaced_schema_fence_preserves_original_tokens(self):
        old = '```yaml\n1: alpha\n2: beta\n```\n'
        target = '```yaml\n1: gamma\n2: delta\n```\n'
        dispositions, receipt, candidate_files = self.cross_file_evidence(
            old, '', status='changed', target=target,
            candidate_files={'spec/new.md': target})
        errors = p.check(old, '', dispositions, receipt, candidate_files=candidate_files)
        self.assertTrue(any('schema' in error for error in errors), errors)

    def test_review_receipt_binds_complete_candidate_file_set(self):
        old = 'Readers MUST preserve `field`.\n'
        dispositions, receipt, candidate_files = self.cross_file_evidence(
            old, '', status='changed', target=old,
            candidate_files={'spec/new.md': old, 'spec/other.md': 'Unrelated text.\n'})
        candidate_files['spec/other.md'] = 'Changed unrelated text.\n'
        errors = p.check(old, '', dispositions, receipt, candidate_files=candidate_files)
        self.assertTrue(any('different evidence' in error for error in errors), errors)


if __name__ == '__main__':
    unittest.main()
