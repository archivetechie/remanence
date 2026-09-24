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


if __name__ == '__main__':
    unittest.main()
